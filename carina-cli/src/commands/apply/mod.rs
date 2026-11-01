pub(crate) mod saved_plan;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::output_style::Colorize;

use futures::stream::{self, StreamExt};

use carina_core::binding_index::{
    LivePreDataSourceReadInputs, ResolvedBindings, SavedPreApplyInputs, WaitAliasSpec,
};
use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::data_source_value::{DataSourceValues, ExecutableDataSourceValues};
use carina_core::differ::Orphans;
use carina_core::executor::{
    AcceptedRequest, ExecutionCheckpointSink, ExecutionObserver, ExecutionOutcome, ExecutionResult,
    LiveExecutionInput, ProviderCheckInputs, SavedExecutionInput, UnresolvedResource,
};
use carina_core::plan::Plan;
use carina_core::provider::AppliedIdentifier;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer, ProviderRouter};
#[cfg(test)]
use carina_core::resource::ConcreteValue;
use carina_core::resource::{
    CanonicalComparisonAttributes, CompositionSourceIdentityResolver, Current, DataSource,
    ModuleSource, ResolvedResource, Resource, ResourceId, SavedDataSource, SavedModuleSource,
    State, Value,
};
use carina_core::shutdown::ShutdownToken;
use carina_core::value::format_value;
use carina_state::{BackendLock, LockInfo, RowBasis, StateBackend, StateFile};

#[cfg(test)]
use carina_core::parser::ProviderConfig;
use carina_core::parser::{
    ExpandedProviderConfig, ExpandedUpstreamState, LoadedProviderContext, ProviderContext,
    ResolvedProviderConfigs,
};

use super::{DriftCommand, verify_for_mutation};
use crate::DetailLevel;
use crate::commands::plan::{
    CheckExportsPreApplyInput, PlanFile, PreparedPlanIdentityData, ProviderUpstreamScope,
    ReloadedCompositionRehydrationError, ReloadedPlanFile, ResolvedProviderInputs,
    SavedDataSourceValue, UpstreamMissingStatePolicy, collect_historical_display_attributes,
    load_and_resolve_provider_configs, prepare_plan_identity_and_provider_inputs,
};
use crate::commands::shared::effect_execution::{
    execute_import_effects, execute_state_only_effects,
};
use crate::commands::shared::finalize::{
    LockCommandOutcome, StatePersistence, finalize_after_execute, finish_lock_command,
};
use crate::commands::shared::incremental_state_saver::{
    FinalStateBase, IncrementalStateSaver, MidRunStateWriter,
};
use crate::commands::shared::lock_heartbeat::{
    HeartbeatOutcome, LockingDisabled, StateLock, StateLockRef, with_lock_heartbeat,
};
use crate::commands::shared::observer::{CliObserver, partial_read_detail_lines};
use crate::commands::shared::plan_errors::render_plan_errors_and_abort;
use crate::commands::shared::progress::{
    ProgressBackend, ProgressSink, RefreshPhase, emit_newline_on_interrupt, format_duration,
};
use crate::commands::shared::state_writeback::{
    ApplyStateSaveInputs, ExportResolutionInputs, FinalizeApplyInput, PostApplyStates,
    ResolveExportValuesPostApplyInput, SkippedExports, apply_persisted_directive_changes,
    materialize_exports, persisted_directive_changes, resolve_export_values_post_apply,
};
use crate::commands::state::map_lock_error;
use crate::cursor::CursorReveal;
use crate::display::print_plan;
use crate::error::{
    AppError, PlanFileJsonError, SavedPlanDirectiveChangesMismatchError,
    SavedPlanSecretVerificationError,
};
use crate::plan_summary::PlanChangeSummary;
use crate::wiring::{
    AddressedDataSourceReadInputs, ProjectedPhase1RefreshInputs, ProviderLoad, ValidatedConfig,
    create_normalizers_from_configs_with_ctx, create_providers_from_configs_with_ctx,
    get_provider_with_ctx, module_constraint_plan_errors, prepare_default_provider_ready_config,
    read_managed_state_with_retry, refresh_loaded_orphan_with_retry,
};
#[cfg(test)]
use crate::wiring::{ValidatedWiring, WiringContext, read_with_retry};
use carina_core::hint::ProjectCommand;
use saved_plan::{
    ExecutablePlanFile, HistoricalResourceAddresses, InspectedPlanFile, ProviderBoundPlanFile,
    ReloadedProviderBoundPlanFile, SavedPlanSecretCandidates, SchemaCheckedPendingPlanFile,
};

/// Re-export ExecutionResult as the public API for apply results.
pub type ApplyResult = ExecutionResult;

type ObserverFactory<'a, S = ModuleSource> = dyn Fn(
        crate::output_style::ColorMode,
        &carina_core::plan::Plan<S>,
        &Path,
    ) -> Box<dyn ExecutionObserver<S>>
    + 'a;

#[derive(serde::Deserialize)]
struct PlanFileHeader {
    version: u32,
    #[serde(rename = "timestamp")]
    _timestamp: serde::de::IgnoredAny,
    #[serde(default)]
    source_path: Option<serde_json::Value>,
}

fn saved_plan_secret_rehydration_error(
    error: carina_core::value::SecretRehydrationSiteError,
    apply_target: ApplyTarget<'_>,
) -> AppError {
    let carina_core::value::SecretRehydrationSiteError { location, source } = error;
    let replan = apply_target.replan_command().to_string();
    let error = match source {
        carina_core::value::SecretRehydrationError::HashMismatch => {
            SavedPlanSecretVerificationError::HashMismatch { location, replan }
        }
        carina_core::value::SecretRehydrationError::EvaluationUnavailable => {
            SavedPlanSecretVerificationError::SecretEvaluationUnavailable { location, replan }
        }
        carina_core::value::SecretRehydrationError::OrphanVanished => {
            SavedPlanSecretVerificationError::OrphanVanished {
                resource: location,
                replan,
            }
        }
        carina_core::value::SecretRehydrationError::Serialization(error) => {
            SavedPlanSecretVerificationError::Serialization {
                location,
                replan,
                error,
            }
        }
        carina_core::value::SecretRehydrationError::ExpectedSecret
        | carina_core::value::SecretRehydrationError::ShapeMismatch => {
            SavedPlanSecretVerificationError::ConfigurationChanged { location, replan }
        }
    };
    AppError::SavedPlanSecretVerification(Box::new(error))
}

fn saved_plan_secret_shape_error(
    location: impl Into<String>,
    apply_target: ApplyTarget<'_>,
) -> AppError {
    saved_plan_secret_rehydration_error(
        carina_core::value::SecretRehydrationSiteError::new(
            location,
            carina_core::value::SecretRehydrationError::ShapeMismatch,
        ),
        apply_target,
    )
}

fn saved_plan_secret_evaluation_error(source: AppError, apply_target: ApplyTarget<'_>) -> AppError {
    AppError::SavedPlanSecretVerification(Box::new(
        SavedPlanSecretVerificationError::EvaluationUnavailable {
            replan: apply_target.replan_command().to_string(),
            error: Box::new(source),
        },
    ))
}

fn saved_plan_secret_preparation_error(
    source: AppError,
    apply_target: ApplyTarget<'_>,
) -> AppError {
    match source {
        error @ AppError::SavedPlanProviderScopeMismatch(_) => error,
        error => saved_plan_secret_evaluation_error(error, apply_target),
    }
}

fn saved_plan_secret_orphan_vanished_error(
    resource: impl Into<String>,
    apply_target: ApplyTarget<'_>,
) -> AppError {
    saved_plan_secret_rehydration_error(
        carina_core::value::SecretRehydrationSiteError::new(
            resource,
            carina_core::value::SecretRehydrationError::OrphanVanished,
        ),
        apply_target,
    )
}

fn cli_observer_factory<S: Send + Sync + 'static>(
    color_mode: crate::output_style::ColorMode,
    plan: &carina_core::plan::Plan<S>,
    project_dir: &Path,
) -> Box<dyn ExecutionObserver<S>> {
    Box::new(CliObserver::new(color_mode, plan, project_dir))
}

fn format_total_apply_line(elapsed: Duration) -> String {
    format!("Done in {}.", format_duration(elapsed))
}

struct BootstrapCreateContext<'a> {
    backend: &'a dyn StateBackend,
    parsed: &'a carina_core::parser::ExpandedInferredFile,
    provider_configs: &'a ResolvedProviderConfigs,
    normalizer: &'a dyn ProviderNormalizer,
    factories: &'a [Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &'a carina_core::schema::SchemaRegistry,
}

async fn create_checked_bootstrap_resource(
    color_mode: crate::output_style::ColorMode,
    operation_provider: &dyn Provider,
    comparison_provider: &ProviderRouter,
    resource: Resource,
    context: BootstrapCreateContext<'_>,
) -> Result<(), AppError> {
    let BootstrapCreateContext {
        backend,
        parsed,
        provider_configs,
        normalizer,
        factories,
        schemas,
    } = context;
    let id = resource.id.clone();
    let bindings = bootstrap_resolved_bindings(parsed);
    let module_gate =
        carina_core::executor::ModuleConstraintGate::from_live_compositions(&parsed.compositions);
    let preparation = carina_core::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        provider_configs.as_provider_configs(),
        normalizer,
        factories,
        schemas,
    );
    let request = carina_core::executor::prepare_create_request(resource, &preparation)
        .await
        .map_err(AppError::from_state_bucket_preparation)?;
    let resolved = request.resource().as_inner().clone();
    let resolved_comparison = comparison_provider.comparison_for(schemas, &resolved.id)?;
    let owner_schema = resolved_comparison.owner_schema();
    let comparison = resolved_comparison.context();
    let schema = Some(owner_schema.schema().as_schema());
    let accepted_request = AcceptedRequest::from_create_request(&request);
    let (create, create_cancellation) =
        carina_core::executor::dispatch_create(operation_provider, &id, request);
    let outcome = create.await;
    // Bootstrap creates are not part of the parallel executor, but they still
    // retain the reporter until the guest call has settled.
    drop(create_cancellation);
    match outcome {
        Ok(outcome) => {
            let identifier = outcome.identifier().as_str().to_string();
            let (state, accepted_request, diagnostic, _read_back_end) =
                carina_core::executor::complete_create_for_writeback(outcome, accepted_request);
            let row = carina_state::ResourceState::from_provider_state_for_resource_and_schema(
                &resolved,
                &state,
                None,
                schema,
                RowBasis::Applied(&accepted_request),
                None,
                comparison,
            )
            .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source))?;
            seed_protected_bootstrap_resource(backend, row)
                .await
                .map_err(|source| {
                    let resource_address = format!(
                        "{} {}",
                        resolved.id.display_type(),
                        carina_core::parser::display_string_literal(resolved.identity_str())
                    );
                    AppError::StateBucketSeed(Box::new(crate::error::StateBucketSeedError::new(
                        identifier.clone(),
                        resource_address,
                        source,
                    )))
                })?;
            if let Some(diagnostic) = diagnostic {
                eprintln!(
                    "{}",
                    format_partial_state_bucket_create_warning(
                        color_mode,
                        &identifier,
                        &diagnostic
                    )
                );
            }
            Ok(())
        }
        Err(error) => {
            let row = match error.kind() {
                carina_core::provider::CreateFailureKind::Untracked(_error) => Ok(None),
                carina_core::provider::CreateFailureKind::OutcomeUnknown {
                    unknown,
                    error: _provider_error,
                } => Ok(Some(
                    carina_state::ResourceState::from_unknown_create_outcome(
                        &resolved, unknown, None, schema, comparison,
                    ),
                )),
                carina_core::provider::CreateFailureKind::Created {
                    created,
                    error: _provider_error,
                } => carina_state::ResourceState::from_observation(
                    &resolved, created, None, schema, None, comparison,
                )
                .map(Some)
                .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source)),
                carina_core::provider::CreateFailureKind::InFlight {
                    pending,
                    error: _provider_error,
                } => carina_state::ResourceState::from_in_flight(
                    &resolved, pending, None, schema, None, comparison,
                )
                .map(Some)
                .map_err(|source| AppError::from_resource_state_encoding(id.clone(), source)),
            };
            let seed_error = match row {
                Ok(Some(row)) => seed_protected_bootstrap_resource(backend, row)
                    .await
                    .err()
                    .map(AppError::Backend),
                Ok(None) => None,
                Err(seed_error) => Some(seed_error),
            };
            Err(AppError::StateBucketCreate(
                crate::error::StateBucketCreateError::new(error, seed_error),
            ))
        }
    }
}

#[cfg(test)]
async fn create_checked_bootstrap_resource_for_test(
    color_mode: crate::output_style::ColorMode,
    operation_provider: &dyn Provider,
    resource: Resource,
    context: BootstrapCreateContext<'_>,
) -> Result<(), AppError> {
    let (schemas, comparison_provider) = carina_core::provider::comparison_fixture_for_test(
        context.schemas,
        std::iter::once(&resource.id),
    );
    let BootstrapCreateContext {
        backend,
        parsed,
        provider_configs,
        normalizer,
        factories,
        schemas: _schemas,
    } = context;
    create_checked_bootstrap_resource(
        color_mode,
        operation_provider,
        &comparison_provider,
        resource,
        BootstrapCreateContext {
            backend,
            parsed,
            provider_configs,
            normalizer,
            factories,
            schemas: &schemas,
        },
    )
    .await
}

async fn seed_protected_bootstrap_resource(
    backend: &dyn StateBackend,
    mut row: carina_state::ResourceState,
) -> carina_state::BackendResult<()> {
    row.protected = true;
    let mut seed = StateFile::new();
    seed.upsert_resource(row)?;
    backend.write_state(&seed).await
}

fn format_partial_state_bucket_create_warning(
    color_mode: crate::output_style::ColorMode,
    identifier: &str,
    diagnostic: &carina_core::provider::PartialReadDiagnostic,
) -> String {
    let [reason, missing, state_recorded] = partial_read_detail_lines(diagnostic);
    format!(
        "{} State bucket '{identifier}' was created with a partial read:\n      {} {reason}\n      {} {missing}\n      {} {state_recorded}",
        "⚠".yellow(color_mode),
        "→".yellow(color_mode),
        "→".yellow(color_mode),
        "→".yellow(color_mode),
    )
}

fn bootstrap_resolved_bindings(
    parsed: &carina_core::parser::ExpandedInferredFile,
) -> ResolvedBindings {
    let managed_current = HashMap::new();
    let remote_bindings = HashMap::new();
    let wait_aliases: Vec<WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(WaitAliasSpec::from)
        .collect();
    ResolvedBindings::pre_data_source_read_live(LivePreDataSourceReadInputs {
        managed: &parsed.resources,
        compositions: &parsed.compositions,
        data_sources: &parsed.data_sources,
        managed_current: &managed_current,
        remote_bindings: &remote_bindings,
        wait_aliases: &wait_aliases,
    })
}

fn split_execution_outcome<S>(outcome: ExecutionOutcome<S>) -> (ExecutionResult<S>, bool) {
    match outcome {
        ExecutionOutcome::Completed(result) => (result, false),
        ExecutionOutcome::Cancelled(result) => (result, true),
    }
}

fn start_apply_state_saver<S: Clone + Send + Sync + 'static>(
    state_file: Option<StateFile>,
    writer: MidRunStateWriter,
    state_save_inputs: Arc<ApplyStateSaveInputs<S>>,
) -> IncrementalStateSaver<ExecutionResult<S>> {
    IncrementalStateSaver::start(
        state_file.unwrap_or_default(),
        writer,
        move |state, result: &ExecutionResult<S>| state_save_inputs.build_state(state, result),
    )
}

fn finalize_module_constraints(
    compositions: &[carina_core::resource::LiveComposition],
    bindings: &ResolvedBindings,
) -> Result<(), AppError> {
    carina_core::executor::ModuleConstraintGate::from_live_compositions(compositions)
        .finish(bindings, true)
        .map_err(AppError::from)
}

fn finalize_saved_module_constraints(
    compositions: &[carina_core::resource::SavedComposition],
    bindings: &ResolvedBindings,
) -> Result<(), AppError> {
    carina_core::executor::ModuleConstraintGate::from_saved_compositions(compositions)
        .finish(bindings, true)
        .map_err(AppError::from)
}

fn data_source_values_from_saved_plan<S: Clone>(
    data_sources: &[carina_core::resource::DataSource<S>],
    saved_values: &[SavedDataSourceValue],
) -> Result<DataSourceValues<S>, AppError> {
    let mut by_id = HashMap::with_capacity(saved_values.len());
    for saved in saved_values {
        if by_id
            .insert(saved.id.clone(), saved.value.clone())
            .is_some()
        {
            return Err(AppError::Config(format!(
                "saved plan has duplicate outcomes for data source `{}`; re-run the plan",
                saved.id
            )));
        }
    }
    let values = DataSourceValues::try_from_data_sources(data_sources, |data_source| {
        by_id.remove(&data_source.id).ok_or_else(|| {
            AppError::Config(format!(
                "saved plan has no outcome for data source `{}`; re-run the plan",
                data_source.id
            ))
        })
    })?;
    if let Some(unexpected) = by_id.keys().next() {
        return Err(AppError::Config(format!(
            "saved plan has an outcome for unknown data source `{unexpected}`; re-run the plan"
        )));
    }
    Ok(values)
}

fn saved_plan_unresolved_by_resolved_id<S: Clone>(
    sorted_resources: &[carina_core::resource::Resource<S>],
    unresolved_resources: &[carina_core::resource::Resource<S>],
    apply_target: ApplyTarget<'_>,
) -> Result<HashMap<ResourceId, UnresolvedResource<S>>, AppError> {
    if sorted_resources.len() != unresolved_resources.len() {
        return Err(AppError::Config(format!(
            "Saved plan resource snapshot mismatch: {} sorted resources but {} unresolved resources. Re-run `{}`.",
            sorted_resources.len(),
            unresolved_resources.len(),
            apply_target.replan_command()
        )));
    }

    let mut by_id = HashMap::with_capacity(sorted_resources.len());
    for (resolved, unresolved) in sorted_resources.iter().zip(unresolved_resources) {
        if resolved.id.provider != unresolved.id.provider
            || resolved.id.resource_type != unresolved.id.resource_type
            || resolved.id.provider_instance != unresolved.id.provider_instance
        {
            return Err(AppError::Config(format!(
                "Saved plan resource snapshot mismatch for {} and {}. Re-run `{}`.",
                resolved.id,
                unresolved.id,
                apply_target.replan_command()
            )));
        }

        let mut source = unresolved.clone();
        source.id = resolved.id.clone();
        if by_id
            .insert(
                resolved.id.clone(),
                UnresolvedResource::from_pre_resolve(source),
            )
            .is_some()
        {
            return Err(AppError::Config(format!(
                "Saved plan contains duplicate resource id {}. Re-run `{}`.",
                resolved.id,
                apply_target.replan_command()
            )));
        }
    }

    Ok(by_id)
}

fn can_use_export_only_fast_path<S: Clone>(
    plan: &carina_core::plan::Plan<S>,
    data_source_values: &ExecutableDataSourceValues<S>,
) -> bool {
    !plan.has_mutations() && !data_source_values.has_deferred_reads()
}

#[derive(Clone, Copy)]
pub(crate) enum ApplyTarget<'a> {
    Project(&'a Path),
    SavedPlan {
        plan_file: &'a Path,
        source_dir: &'a Path,
    },
}

impl<'a> ApplyTarget<'a> {
    fn source_dir(self) -> &'a Path {
        match self {
            Self::Project(dir) => dir,
            Self::SavedPlan { source_dir, .. } => source_dir,
        }
    }

    fn replan_command(self) -> ProjectCommand<'a> {
        match self {
            Self::Project(dir) => ProjectCommand::new("plan", dir),
            Self::SavedPlan {
                plan_file,
                source_dir,
            } => ProjectCommand::new("plan --out", source_dir).with_path_argument(plan_file),
        }
    }

    fn apply_path(self) -> &'a Path {
        match self {
            Self::Project(dir) => dir,
            Self::SavedPlan { plan_file, .. } => plan_file,
        }
    }
}

fn check_legacy_name_overrides(
    state_file: &StateFile,
    accept: bool,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    if !state_file.has_legacy_name_overrides() || accept {
        return Ok(());
    }

    let mut affected: Vec<String> = state_file
        .legacy_name_override_resources()
        .into_iter()
        .map(|resource| {
            format!(
                "{}.{}.{}",
                resource.provider, resource.resource_type, resource.identity
            )
        })
        .collect();
    affected.sort();
    affected.dedup();

    Err(AppError::Validation(format!(
        "State file contains pre-Phase-5 legacy name overrides without recorded original DSL values for: {}.\n\
         Run `{}` first to inspect the drift, or re-run \
         `{} --accept-legacy-name-overrides` to proceed.",
        affected.join(", "),
        apply_target.replan_command(),
        ProjectCommand::new("apply", apply_target.apply_path())
    )))
}

/// Recover the project directory recorded by plan versions that stored a
/// single `.crn` source file. Current plan versions record the directory and
/// must use that raw path unchanged.
fn saved_plan_replan_project_dir(source_path: &Path) -> &Path {
    if source_path
        .extension()
        .is_some_and(|extension| extension == "crn")
    {
        source_path.parent().unwrap_or_else(|| Path::new(""))
    } else {
        source_path
    }
}

fn saved_plan_replan_command(plan_path: &Path, source_path: &Path) -> String {
    ApplyTarget::SavedPlan {
        plan_file: plan_path,
        source_dir: saved_plan_replan_project_dir(source_path),
    }
    .replan_command()
    .to_string()
}

#[allow(clippy::too_many_arguments)]
async fn execute_effects_with_observer(
    plan: &Plan,
    provider: &ProviderRouter,
    normalizer: &dyn ProviderNormalizer,
    provider_configs: &[ExpandedProviderConfig],
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
    bindings: &mut ResolvedBindings,
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_values: ExecutableDataSourceValues,
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    data_source_origins: &[DataSource],
    compositions: &[carina_core::resource::LiveComposition],
    precompleted_success_count: usize,
    checkpoint_sink: ExecutionCheckpointSink,
    cancel: ShutdownToken,
    parallelism: NonZeroUsize,
    observer: &dyn ExecutionObserver,
) -> ExecutionOutcome {
    let input = LiveExecutionInput {
        plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: unresolved_resources,
            data_source_origins,
        },
        compositions,
        bindings: bindings.clone(),
        managed_current: std::mem::take(managed_current),
        data_source_values,
        normalizer,
        provider_configs,
        factories,
        schemas,
        parallelism,
        precompleted_success_count,
        checkpoint_sink,
    };

    let outcome = carina_core::executor::execute_live_plan(provider, input, observer, cancel).await;

    // Write back the updated current views so callers see refreshes.
    let result = match &outcome {
        ExecutionOutcome::Completed(result) | ExecutionOutcome::Cancelled(result) => result,
    };
    *managed_current = result.managed_current.clone();
    *bindings = result.bindings.clone();

    outcome
}

/// Saved-plan executor gateway. Requiring [`ExecutablePlanFile`] here makes it
/// impossible for the deserialized `PlanFile` path to dispatch effects before
/// schema compatibility and all typed secret placeholders have been verified.
#[allow(clippy::too_many_arguments)]
async fn execute_rehydrated_saved_plan(
    plan_file: &ExecutablePlanFile,
    provider: &ProviderRouter,
    normalizer: &dyn ProviderNormalizer,
    provider_configs: &[ExpandedProviderConfig],
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
    bindings: &mut ResolvedBindings,
    managed_current: &mut HashMap<ResourceId, Current>,
    data_source_values: ExecutableDataSourceValues<SavedModuleSource>,
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource<SavedModuleSource>>,
    data_source_origins: &[SavedDataSource],
    compositions: &[carina_core::resource::SavedComposition],
    precompleted_success_count: usize,
    checkpoint_sink: ExecutionCheckpointSink<SavedModuleSource>,
    cancel: ShutdownToken,
    parallelism: NonZeroUsize,
    observer: &dyn ExecutionObserver<SavedModuleSource>,
) -> ExecutionOutcome<SavedModuleSource> {
    let input = SavedExecutionInput {
        plan: plan_file.plan(),
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: unresolved_resources,
            data_source_origins,
        },
        compositions,
        bindings: bindings.clone(),
        managed_current: std::mem::take(managed_current),
        data_source_values,
        normalizer,
        provider_configs,
        factories,
        schemas,
        parallelism,
        precompleted_success_count,
        checkpoint_sink,
    };
    let outcome =
        carina_core::executor::execute_saved_plan(provider, input, observer, cancel).await;
    let result = match &outcome {
        ExecutionOutcome::Completed(result) | ExecutionOutcome::Cancelled(result) => result,
    };
    *managed_current = result.managed_current.clone();
    *bindings = result.bindings.clone();
    outcome
}

/// Re-load each upstream declared in the saved plan and verify its
/// attribute map matches the snapshot the plan was computed against
/// (#2303). Fails on the first drifted binding so the user gets an
/// actionable message naming what changed; pretending the apply will
/// succeed and silently mixing plan-time and apply-time values
/// produces incorrect cascade re-resolution.
async fn verify_upstream_snapshot(
    runtime: &crate::runtime::CommandRuntime,
    sources: &[crate::commands::plan::UpstreamSource],
    snapshot: &HashMap<String, HashMap<String, Value>>,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    let project_dir = apply_target.source_dir();
    let upstream_states: Vec<ExpandedUpstreamState> = sources
        .iter()
        .map(|s| ExpandedUpstreamState {
            binding: s.binding.clone(),
            source: s.source.clone(),
        })
        .collect();
    let provider_context = ProviderContext::default();
    let mut cycle_guard = super::plan::seed_cycle_guard(project_dir);
    let current = super::plan::load_upstream_states(
        runtime,
        &upstream_states,
        project_dir,
        &provider_context,
        &mut cycle_guard,
        super::plan::UpstreamMissingStatePolicy::Strict,
    )
    .await?;

    diff_upstream_snapshot(snapshot, &current, apply_target).map_err(AppError::Config)
}

/// Pure comparison between a planned snapshot and a freshly-loaded
/// upstream view. Returns the user-facing error message naming the
/// first binding that disagrees, or `Ok(())` when the two views are
/// equal.
///
/// Split out so it can be unit-tested without a real upstream backend.
fn diff_upstream_snapshot(
    snapshot: &HashMap<String, HashMap<String, Value>>,
    current: &HashMap<String, HashMap<String, Value>>,
    apply_target: ApplyTarget<'_>,
) -> Result<(), String> {
    for (binding, planned_attrs) in snapshot {
        match current.get(binding) {
            None => {
                return Err(format!(
                    "upstream_state '{}' was present at plan time but is missing now. \
                     Re-run `{}` to capture the current upstream view.",
                    binding,
                    apply_target.replan_command()
                ));
            }
            Some(current_attrs) if current_attrs != planned_attrs => {
                return Err(format!(
                    "upstream_state '{}' has drifted since the plan was created. \
                     Re-run `{}` so the apply uses the values it was computed against.",
                    binding,
                    apply_target.replan_command()
                ));
            }
            Some(_) => {}
        }
    }
    for binding in current.keys() {
        if !snapshot.contains_key(binding) {
            return Err(format!(
                "upstream_state '{}' was added since the plan was created. \
                 Re-run `{}` so the apply sees the new upstream binding.",
                binding,
                apply_target.replan_command()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod upstream_snapshot_tests {
    use super::*;

    fn binding(name: &str, attrs: &[(&str, &str)]) -> (String, HashMap<String, Value>) {
        let map = attrs
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    Value::Concrete(ConcreteValue::String(v.to_string().into())),
                )
            })
            .collect();
        (name.to_string(), map)
    }

    #[test]
    fn matching_snapshot_passes() {
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current = snapshot.clone();
        assert!(
            diff_upstream_snapshot(&snapshot, &current, ApplyTarget::Project(Path::new(".")))
                .is_ok()
        );
    }

    #[test]
    fn drifted_attribute_fails() {
        // Same binding name, but the attribute value changed underneath.
        // This is the original bug: cascade re-resolution would silently
        // see "vpc-B" while the static plan was built around "vpc-A".
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-B")])]
                .into_iter()
                .collect();
        let err = diff_upstream_snapshot(&snapshot, &current, ApplyTarget::Project(Path::new(".")))
            .expect_err("must fail");
        assert!(
            err.contains("network"),
            "error must name the binding: {err}"
        );
        assert!(err.contains("drifted"), "error must say drifted: {err}");
    }

    #[test]
    fn binding_disappeared_fails() {
        let snapshot: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let current: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let err = diff_upstream_snapshot(&snapshot, &current, ApplyTarget::Project(Path::new(".")))
            .expect_err("must fail");
        assert!(err.contains("missing now"), "got: {err}");
    }

    #[test]
    fn binding_appeared_fails() {
        let snapshot: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let current: HashMap<String, HashMap<String, Value>> =
            vec![binding("network", &[("vpc_id", "vpc-A")])]
                .into_iter()
                .collect();
        let err = diff_upstream_snapshot(&snapshot, &current, ApplyTarget::Project(Path::new(".")))
            .expect_err("must fail");
        assert!(err.contains("added since"), "got: {err}");
    }

    #[test]
    fn empty_both_sides_passes() {
        let empty: HashMap<String, HashMap<String, Value>> = HashMap::new();
        assert!(
            diff_upstream_snapshot(&empty, &empty, ApplyTarget::Project(Path::new("."))).is_ok()
        );
    }
}

/// Refresh states for resources whose operations failed.
///
/// This is kept for use by tests in `tests.rs`. The core executor has its own
/// internal version.
#[cfg(test)]
pub async fn refresh_pending_states(
    color_mode: crate::output_style::ColorMode,
    provider: &dyn Provider,
    managed_current: &mut HashMap<ResourceId, Current>,
    pending_refreshes: &HashMap<ResourceId, AppliedIdentifier>,
) -> HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return HashSet::new();
    }

    println!();
    println!(
        "{}",
        "Refreshing uncertain resource states...".cyan(color_mode)
    );

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    let mut failed_refreshes = HashSet::new();

    for (id, identifier) in refreshes {
        match read_with_retry(provider, id, identifier).await {
            Ok(state) => {
                println!("  {} Refresh {}", "✓".green(color_mode), id);
                managed_current.insert(id.clone(), Current::from_state(state));
            }
            Err(error) => {
                println!("  {} Refresh {} - {}", "!".yellow(color_mode), id, error);
                failed_refreshes.insert(id.clone());
            }
        }
    }

    failed_refreshes
}

/// Save state after apply. Does NOT release the lock -- caller is responsible.
///
/// When locking is explicitly disabled, state is written without lock
/// validation via `save_state_unlocked` and its required witness.
///
/// Returns the [`SkippedExports`] summary after state is saved. Callers must fold
/// unresolved exports into the command's exit status (carina#3710).
pub(crate) async fn finalize_apply<S: Clone>(
    color_mode: crate::output_style::ColorMode,
    input: FinalizeApplyInput<'_, S>,
    persistence: StatePersistence,
) -> Result<SkippedExports, AppError> {
    println!();
    println!("{}", "Saving state...".cyan(color_mode));

    let mut state = input
        .state_save_inputs
        .build_state(input.state_base.into_state(), input.result)?;

    // `Some` with empty export params is meaningful: the user removed the `exports {}` block,
    // so stale entries the previous apply persisted must be cleared
    // (#2932). `None` preserves `state.exports` because the caller has
    // no source-side view of which exports the user intends — see the
    // `FinalizeApplyInput::export_resolution_inputs` doc-comment.
    let export_resolution = if let Some(export_inputs) = input.export_resolution_inputs {
        let post_apply_states = PostApplyStates::from_current_and_state(
            &input.result.managed_current,
            export_inputs.data_source_values,
            &state,
        );
        resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
            export_params: export_inputs.export_params,
            sorted_resources: export_inputs.resources,
            pre_resolve_compositions: input.pre_resolve_compositions,
            post_apply_states: &post_apply_states,
            schemas: input.state_save_inputs.schemas(),
            wait_aliases: input.wait_aliases,
            provider_context: export_inputs.provider_context,
        })
        .and_then(|values| materialize_exports(&values))
        .map(|resolution| resolution.write_into(color_mode, &mut state))
    } else {
        Ok(SkippedExports::default())
    };

    // A provider mutation has already happened. Export materialization is a
    // secondary concern, so an export error must not prevent durable resource
    // writeback. Save the resource state first, then return the original
    // export error to the caller.
    match input.lock {
        StateLockRef::Held(lock) => {
            save_state_locked_after_execute(input.backend, lock, &mut state, persistence).await?;
        }
        StateLockRef::Disabled(disabled) => {
            save_state_unlocked_after_execute(input.backend, disabled, &mut state, persistence)
                .await?;
        }
    }
    println!(
        "  {} State saved (serial: {})",
        "✓".green(color_mode),
        state.serial
    );

    export_resolution
}

/// Renew the lock and write state with lock validation.
///
/// This ensures that the lock is still held before writing state, preventing
/// silent state corruption when a lock has expired and been acquired by another
/// process during a long-running operation.
pub(crate) async fn save_state_locked(
    backend: &dyn StateBackend,
    lock: &LockInfo,
    state: &mut StateFile,
) -> Result<(), AppError> {
    let renewed = backend.renew_lock(lock).await.map_err(AppError::Backend)?;
    state.increment_serial();
    backend
        .write_state_locked(state, &renewed)
        .await
        .map_err(AppError::Backend)
}

pub(crate) async fn save_state_locked_after_execute(
    backend: &dyn StateBackend,
    lock: &LockInfo,
    state: &mut StateFile,
    persistence: StatePersistence,
) -> Result<(), AppError> {
    match persistence {
        StatePersistence::Normal => save_state_locked(backend, lock, state).await,
        StatePersistence::CancellationCleanup => {
            state.increment_serial();
            backend
                .write_state_locked_for_cleanup(state, lock)
                .await
                .map_err(AppError::Backend)
        }
    }
}

/// Write state without lock validation.
///
/// Used when `--lock=false` is specified. Increments the serial number and
/// writes using `write_state` (no lock check).
pub(crate) async fn save_state_unlocked(
    backend: &dyn StateBackend,
    _disabled: &LockingDisabled,
    state: &mut StateFile,
) -> Result<(), AppError> {
    state.increment_serial();
    backend.write_state(state).await.map_err(AppError::Backend)
}

pub(crate) async fn save_state_unlocked_after_execute(
    backend: &dyn StateBackend,
    disabled: &LockingDisabled,
    state: &mut StateFile,
    persistence: StatePersistence,
) -> Result<(), AppError> {
    match persistence {
        StatePersistence::Normal => save_state_unlocked(backend, disabled, state).await,
        StatePersistence::CancellationCleanup => {
            state.increment_serial();
            backend
                .write_state_for_cleanup(state)
                .await
                .map_err(AppError::Backend)
        }
    }
}

/// Read state from the backend and, if `check_and_migrate` lifted the
/// on-disk schema in memory, persist the upgraded shape immediately
/// under the current lock.
///
/// This is the lock-held entry-point used by `apply`, `destroy`, and
/// `state refresh`. Without it, the no-op short-circuits inside those
/// commands (e.g. "No changes needed." when no resource diff exists)
/// would skip the writer entirely and leave the on-disk version
/// stuck at the older schema — which is the bug reported in
/// carina#3315 and which makes carina#3283's "Disk state will be
/// rewritten during the next apply or state refresh of that project."
/// warning text a lie.
///
/// Persistence happens exactly once per state file's schema lifetime:
/// after the first successful run under this helper the on-disk
/// version matches `StateFile::CURRENT_VERSION`, so subsequent
/// `read_state` calls produce a pristine `LoadedState` and this
/// helper is a no-op write-wise.
pub(crate) async fn load_state_persist_if_migrated(
    backend: &dyn StateBackend,
    lock: StateLockRef<'_>,
) -> Result<Option<StateFile>, AppError> {
    match backend.read_state().await.map_err(AppError::Backend)? {
        None => Ok(None),
        Some(carina_state::LoadedState::Pristine(state)) => Ok(Some(state)),
        Some(carina_state::LoadedState::Migrated { mut state, .. }) => {
            match lock {
                StateLockRef::Held(lock) => save_state_locked(backend, lock, &mut state).await?,
                StateLockRef::Disabled(disabled) => {
                    save_state_unlocked(backend, disabled, &mut state).await?
                }
            }
            Ok(Some(state))
        }
    }
}

/// Remove pre-#3181 identifier-less data-source rows identified by the
/// parser's typed `read` addresses and persist the normalized state while the
/// caller's lock is held.
///
/// This is intentionally separate from JSON migration: identifier-less
/// managed rows and historical data-source rows have the same old wire shape,
/// so cleanup requires both that shape and the parsed resource kind/address.
/// An identifier-bearing managed row is preserved even when a data source now
/// occupies the same address.
pub(crate) async fn prune_legacy_data_source_rows_persist_if_changed(
    backend: &dyn StateBackend,
    lock: StateLockRef<'_>,
    state_file: &mut Option<StateFile>,
    data_sources: &[DataSource],
) -> Result<(), AppError> {
    let Some(state) = state_file.as_mut() else {
        return Ok(());
    };
    if state.remove_legacy_data_source_rows(data_sources) == 0 {
        return Ok(());
    }
    match lock {
        StateLockRef::Held(lock) => save_state_locked(backend, lock, state).await?,
        StateLockRef::Disabled(disabled) => save_state_unlocked(backend, disabled, state).await?,
    }
    Ok(())
}

/// Persist export changes when the plan has no mutating effects.
///
/// Used when `!plan.has_mutations()` short-circuits apply: no
/// managed-resource Create/Update/Delete is needed (the plan is
/// either empty or carries only `Read`/`Wait` effects), but
/// exports may have changed. Rebuild the exports from the current
/// state + desired `export_params` and write the state
/// (carina#3270).
///
/// If any export remains unresolved, state is saved first, then this prints
/// `! N export(s) not written` instead of `✓ Exports updated` and returns an
/// error so the command exits non-zero (carina#3710).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_exports_only(
    color_mode: crate::output_style::ColorMode,
    backend: &dyn StateBackend,
    lock: StateLockRef<'_>,
    state_file: Option<StateFile>,
    sorted_resources: &[Resource],
    data_source_values: &DataSourceValues,
    pre_resolve_compositions: &[carina_core::resource::LiveComposition],
    export_params: &carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::ExpandedInferredExportParam,
    >,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[carina_core::binding_index::WaitAliasSpec],
    managed_current: &HashMap<ResourceId, Current>,
    provider_context: &LoadedProviderContext,
) -> Result<(), AppError> {
    let mut state = state_file.unwrap_or_default();
    let post_apply_states =
        PostApplyStates::from_current_and_state(managed_current, data_source_values, &state);
    let values = resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
        export_params,
        sorted_resources,
        pre_resolve_compositions,
        post_apply_states: &post_apply_states,
        schemas,
        wait_aliases,
        provider_context,
    })?;
    let resolution = materialize_exports(&values)?;
    let skipped_exports = resolution.write_into(color_mode, &mut state);
    match lock {
        StateLockRef::Held(lock) => save_state_locked(backend, lock, &mut state).await?,
        StateLockRef::Disabled(disabled) => {
            save_state_unlocked(backend, disabled, &mut state).await?
        }
    }
    println!(
        "  {} State saved (serial: {})",
        "✓".green(color_mode),
        state.serial
    );
    if skipped_exports.is_empty() {
        println!("  {} Exports updated", "✓".green(color_mode));
        Ok(())
    } else {
        println!(
            "  {} {}",
            "!".yellow(color_mode),
            skipped_exports.count_description()
        );
        Err(AppError::Config(skipped_exports.failure_message()))
    }
}

async fn persist_directives_only(
    color_mode: crate::output_style::ColorMode,
    backend: &dyn StateBackend,
    lock: StateLockRef<'_>,
    mut state: StateFile,
    directive_change_count: usize,
) -> Result<(), AppError> {
    match lock {
        StateLockRef::Held(lock) => save_state_locked(backend, lock, &mut state).await?,
        StateLockRef::Disabled(disabled) => {
            save_state_unlocked(backend, disabled, &mut state).await?
        }
    }
    println!(
        "  {} State saved (serial: {})",
        "✓".green(color_mode),
        state.serial
    );
    println!(
        "  {} {} resource directive change(s) persisted",
        "✓".green(color_mode),
        directive_change_count
    );
    Ok(())
}

/// Result of the saved-plan drift pass.
///
/// `verified_planned_states` contains the historical snapshot after each
/// placeholder has been checked exactly once against the corresponding fresh
/// provider read. The historical typestate gate consumes this map instead of
/// repeating the Argon2 verification performed here.
#[derive(Debug)]
pub(crate) struct DriftCheckResult {
    drift_messages: Option<Vec<String>>,
    actual_states: HashMap<ResourceId, State>,
    verified_planned_states: HashMap<ResourceId, State>,
}

impl DriftCheckResult {
    pub(crate) fn drift_messages(&self) -> Option<&[String]> {
        self.drift_messages.as_deref()
    }

    fn into_verified_states(self) -> (HashMap<ResourceId, State>, HashMap<ResourceId, State>) {
        debug_assert!(self.drift_messages.is_none());
        (self.actual_states, self.verified_planned_states)
    }
}

/// Detect infrastructure drift and retain the single historical-secret
/// verification pass for the saved-plan gate.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn detect_drift<S: Clone>(
    color_mode: crate::output_style::ColorMode,
    sorted_resources: &[carina_core::resource::Resource<S>],
    planned_state_entries: &[crate::commands::plan::ReloadedCurrentStateEntry],
    historical_addresses: &HistoricalResourceAddresses,
    operation_provider: &dyn Provider,
    comparison_provider: &ProviderRouter,
    state_file: Option<&StateFile>,
    schemas: &carina_core::schema::SchemaRegistry,
    apply_target: ApplyTarget<'_>,
) -> Result<DriftCheckResult, AppError> {
    let mut drift_detected = false;
    let mut drift_messages: Vec<String> = Vec::new();
    let mut actual_states = HashMap::new();
    let mut verified_planned_states = HashMap::new();
    let planned_states: HashMap<_, _> = planned_state_entries
        .iter()
        .map(|entry| match entry {
            crate::commands::plan::ReloadedCurrentStateEntry::Managed { id, current } => {
                (id, current)
            }
        })
        .collect();

    for resource in sorted_resources {
        let resolved_resource = ResolvedResource::resolve_with_schemas(resource.clone(), schemas)?;
        // carina#3181: `sorted_resources` is managed-only — drift
        // detection only operates over provider-backed managed
        // resources.
        let typed_comparison = comparison_provider.comparison_for(schemas, &resource.id)?;
        let planned_snapshot = planned_states.get(&resource.id).copied();
        let planned_current = planned_snapshot.map(|snapshot| snapshot.as_ref());
        let planned_state = planned_current.and_then(|current| match current {
            Current::Absent(state) => Some(state.state()),
            Current::Present(present) => Some(present.state()),
            Current::CreateInFlight(_pending) => None,
            Current::CreateOutcomeUnknown(_unknown) => None,
            Current::MissingIdentifier(state) => Some(state.state()),
        });
        let planned_exists = planned_current.and_then(|current| match current {
            Current::Absent(_absent) => Some(false),
            Current::Present(_present) => Some(true),
            Current::CreateInFlight(_pending) => None,
            Current::CreateOutcomeUnknown(_unknown) => None,
            Current::MissingIdentifier(_unidentified) => Some(true),
        });
        let historical_id = planned_current
            .map(|_| historical_addresses.origin_for(&resource.id))
            .unwrap_or(&resource.id);

        let mut actual_state = match planned_current {
            Some(Current::Present(present)) => read_managed_state_with_retry(
                operation_provider,
                historical_id,
                present.identifier(),
                state_file,
                schemas,
            )
            .await
            .map_err(AppError::Provider)?,
            Some(Current::MissingIdentifier(_state)) => {
                return Err(AppError::Validation(format!(
                    "saved plan resource {} has no valid provider identifier; create a new plan before applying",
                    resource.id
                )));
            }
            Some(Current::CreateInFlight(_pending)) => State::not_found(historical_id.clone()),
            Some(Current::CreateOutcomeUnknown(_unknown)) => {
                State::not_found(historical_id.clone())
            }
            Some(Current::Absent(_state)) => State::not_found(historical_id.clone()),
            None => State::not_found(historical_id.clone()),
        };
        // The provider read uses the pre-move address, but every downstream
        // saved-plan map is keyed by the moved-to runtime address.
        actual_state.id = resource.id.clone();
        actual_states.insert(resource.id.clone(), actual_state.clone());

        if let Some(planned) = planned_state {
            let planned_exists = planned_exists.expect("planned state and variant are paired");
            if planned_exists != actual_state.exists {
                drift_detected = true;
                if planned_exists {
                    drift_messages.push(format!(
                        "  {} {}: resource existed at plan time but no longer exists",
                        "~".yellow(color_mode),
                        resource.id
                    ));
                } else {
                    drift_messages.push(format!(
                        "  {} {}: resource did not exist at plan time but now exists",
                        "~".yellow(color_mode),
                        resource.id
                    ));
                }
            } else if planned_exists && actual_state.exists {
                let verification =
                    carina_core::value::verify_historical_state_secrets(planned, &actual_state)
                        .map_err(|error| {
                            saved_plan_secret_rehydration_error(error, apply_target)
                        })?;
                let secret_drift = verification.drifted_attributes();
                let mut verified_planned = verification.state().clone();
                resolved_resource
                    .ignore_changes()
                    .mask_observed_readable_leaves(
                        &mut verified_planned.attributes,
                        &actual_state.attributes,
                    );
                verified_planned_states.insert(resource.id.clone(), verified_planned.clone());
                let validated_schema = typed_comparison.owner_schema().schema();
                let schema = validated_schema.as_schema();
                let planned_comparison = CanonicalComparisonAttributes::canonicalize(
                    verified_planned.attributes.clone(),
                    schema,
                );
                let actual_comparison = CanonicalComparisonAttributes::canonicalize(
                    actual_state.attributes.clone(),
                    schema,
                );
                // Compare attributes for existing resources
                let mut attr_diffs: Vec<String> = Vec::new();
                for (key, planned_val) in &planned_comparison {
                    let historical_shape = planned_state
                        .and_then(|state| state.attributes.get(key))
                        .unwrap_or(planned_val);
                    let masked_planned = carina_core::value::mask_plaintext_using_secret_shape(
                        planned_val,
                        historical_shape,
                    );
                    match actual_comparison.get(key) {
                        Some(actual_val) if secret_drift.contains(key) => {
                            let masked = carina_core::value::mask_plaintext_using_secret_shape(
                                actual_val,
                                historical_shape,
                            );
                            attr_diffs.push(format!(
                                "      {}: {} → {}",
                                key,
                                format_value(&masked_planned),
                                format_value(&masked)
                            ));
                        }
                        Some(actual_val)
                            if !carina_core::diff_helpers::resource_attribute_values_equal(
                                actual_val,
                                planned_val,
                                Some(validated_schema),
                                key,
                                typed_comparison.context(),
                            ) =>
                        {
                            let masked = carina_core::value::mask_plaintext_using_secret_shape(
                                actual_val,
                                historical_shape,
                            );
                            attr_diffs.push(format!(
                                "      {}: {} → {}",
                                key,
                                format_value(&masked_planned),
                                format_value(&masked)
                            ));
                        }
                        None => {
                            attr_diffs.push(format!(
                                "      {}: {} → (removed)",
                                key,
                                format_value(&masked_planned)
                            ));
                        }
                        _ => {}
                    }
                }
                for (key, actual_val) in &actual_comparison {
                    if !planned_comparison.contains_key(key) {
                        attr_diffs.push(format!(
                            "      {}: (none) → {}",
                            key,
                            format_value(actual_val)
                        ));
                    }
                }
                if !attr_diffs.is_empty() {
                    drift_detected = true;
                    drift_messages.push(format!(
                        "  {} {}: attributes have changed since plan was created:",
                        "~".yellow(color_mode),
                        resource.id
                    ));
                    drift_messages.extend(attr_diffs);
                }
            } else {
                verified_planned_states.insert(resource.id.clone(), planned.clone());
            }
        } else {
            return Err(AppError::Config(format!(
                "Resource {} is present in plan but missing from planned states. \
                 The plan file may be corrupted. Please re-run `{}`.",
                resource.id,
                apply_target.replan_command()
            )));
        }
    }

    let drift_messages = if drift_detected {
        Some(drift_messages)
    } else {
        None
    };
    Ok(DriftCheckResult {
        drift_messages,
        actual_states,
        verified_planned_states,
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn detect_drift_for_test<S: Clone>(
    color_mode: crate::output_style::ColorMode,
    sorted_resources: &[carina_core::resource::Resource<S>],
    planned_state_entries: &[crate::commands::plan::ReloadedCurrentStateEntry],
    historical_addresses: &HistoricalResourceAddresses,
    operation_provider: &dyn Provider,
    state_file: Option<&StateFile>,
    schemas: &carina_core::schema::SchemaRegistry,
    apply_target: ApplyTarget<'_>,
) -> Result<DriftCheckResult, AppError> {
    let (schemas, comparison_provider) = carina_core::provider::comparison_fixture_for_test(
        schemas,
        sorted_resources.iter().map(|resource| &resource.id),
    );
    detect_drift(
        color_mode,
        sorted_resources,
        planned_state_entries,
        historical_addresses,
        operation_provider,
        &comparison_provider,
        state_file,
        &schemas,
        apply_target,
    )
    .await
}

fn drift_replan_hint(apply_target: ApplyTarget<'_>) -> String {
    format!(
        "Please re-run `{}` to create a new plan that reflects the current state.",
        apply_target.replan_command()
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn run_apply(
    runtime: &crate::runtime::CommandRuntime,
    path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    run_apply_with_observer_factory(
        runtime,
        path,
        auto_approve,
        lock,
        parallelism,
        accept_legacy_name_overrides,
        provider_context,
        cancel.clone(),
        &cli_observer_factory,
    )
    .await
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
async fn run_apply_with_observer_factory(
    runtime: &crate::runtime::CommandRuntime,
    path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
) -> Result<(), AppError> {
    let color_mode = runtime.color_mode();
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let duplicate_declarations = loaded.duplicate_declarations;
    let bootstrap_unresolved = loaded.unresolved_parsed.clone();
    let parsed = loaded.parsed;
    let backend_file = loaded.backend_file;

    let base_dir = get_base_dir(path);
    let providers = ProviderLoad::load(runtime, &parsed.providers, base_dir)?;
    let mut validated_config = providers.validate_config_pair(
        parsed,
        loaded.unresolved_parsed,
        base_dir,
        false,
        &duplicate_declarations,
    )?;
    // Wiring needs declaration metadata and schemas before a bootstrap
    // provider may be created. Provider inputs are loaded lazily in that
    // branch; ordinary apply reaches backend verification without touching
    // upstream state and resolves once in the lock-held path.
    let verified_backend = verify_for_mutation(
        path,
        validated_config.resources().parsed().backend.as_ref(),
        DriftCommand::Apply,
    )?;

    // Check for backend configuration - use local backend by default
    let backend: Arc<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?
        .into();

    // Handle bootstrap if S3 backend is configured
    let s3_bootstrap = verified_backend
        .s3_settings()
        .map(|settings| (settings.bucket().to_owned(), settings.auto_create()));
    if let Some((bucket_name, auto_create)) = s3_bootstrap {
        // Check if bucket exists (bootstrap detection)
        let bucket_exists = backend.bucket_exists().await.map_err(AppError::Backend)?;

        if !bucket_exists {
            println!(
                "{}",
                "State bucket not found. Running bootstrap..."
                    .yellow(color_mode)
                    .bold(color_mode)
            );

            // Check if there's a bucket resource defined with matching name
            let backend_resource_type = backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            if let Some(bucket_resource) = validated_config
                .resources()
                .parsed()
                .find_resource_by_attr(backend_resource_type, "bucket", &bucket_name)
            {
                println!("Found state bucket resource in configuration.");
                println!(
                    "Creating bucket '{}' before other resources...",
                    bucket_name.cyan(color_mode)
                );

                // Create the bucket resource using the factory pattern
                let backend_provider_name = backend
                    .provider_name()
                    .ok_or("Backend does not specify a provider name")?;
                // This is the only pre-lock path that constructs a provider.
                // Resolve its attributes here; every other apply path waits
                // for the post-reconciliation lock-held resolution below.
                let bootstrap_provider_inputs = load_apply_provider_inputs(
                    runtime,
                    validated_config.resources().parsed(),
                    base_dir,
                    provider_context,
                )
                .await?;
                let (bucket_provider, bucket_normalizer) = create_state_bucket_bootstrap_provider(
                    &validated_config,
                    bootstrap_provider_inputs.configs(),
                    backend_provider_name,
                )
                .await?;
                // Module expansion can place a module-owned state-bucket
                // resource in `parsed.resources`; `find_resource_by_attr`
                // does not imply top-level ownership. Gate bootstrap with
                // the same expanded compositions and resolved config values
                // that are available before any provider effect runs.
                create_checked_bootstrap_resource(
                    color_mode,
                    &bucket_provider,
                    &bucket_provider,
                    bucket_resource.clone(),
                    BootstrapCreateContext {
                        backend: backend.as_ref(),
                        parsed: validated_config.resources().parsed(),
                        provider_configs: bootstrap_provider_inputs.configs(),
                        normalizer: bucket_normalizer.as_ref(),
                        factories: validated_config.context().factories(),
                        schemas: validated_config.context().schemas(),
                    },
                )
                .await?;
                println!(
                    "  {} Created state bucket: {}",
                    "✓".green(color_mode),
                    bucket_name
                );
            } else {
                // Auto-create the bucket if auto_create is enabled
                if auto_create {
                    println!(
                        "Auto-creating state bucket: {}",
                        bucket_name.cyan(color_mode)
                    );
                    backend.create_bucket().await.map_err(AppError::Backend)?;
                    println!("  {} Created state bucket", "✓".green(color_mode));

                    let backend_provider_name = backend
                        .provider_name()
                        .ok_or("Backend does not specify a provider name")?;

                    // Append resource definition to backend file
                    let target_file = backend_file.clone().unwrap_or_else(|| path.to_path_buf());

                    let resource_code = backend
                        .resource_definition(&bucket_name)
                        .ok_or("Backend does not support resource definition generation")?;

                    // Read existing content if file exists, then append
                    let mut content = if target_file.exists() {
                        fs::read_to_string(&target_file).map_err(|e| {
                            format!("Failed to read {}: {}", target_file.display(), e)
                        })?
                    } else {
                        String::new()
                    };
                    content.push_str(&resource_code);

                    fs::write(&target_file, &content)
                        .map_err(|e| format!("Failed to write {}: {}", target_file.display(), e))?;
                    println!(
                        "  {} Added resource definition to {}",
                        "✓".green(color_mode),
                        target_file.display()
                    );

                    // Re-parse and re-resolve the updated configuration so the
                    // bucket resource is fully addressed (in particular: its
                    // anonymous identifier is computed) before we seed state.
                    // Without this, the seed can't use the resolved name and
                    // the differ produces phantom Create + Delete entries on
                    // the next plan (#2533). `skip_resource_validation = true`
                    // reuses the heavier preflight pipeline only for its
                    // resolve+anon-id steps; the schema validation already
                    // ran on the pre-injection configuration.
                    // Keep the authored pre-injection canonical snapshot for
                    // resource pairing; replace only the resolved snapshot so
                    // the lock-held path sees the injected bucket row.
                    validated_config = reload_apply_configuration_after_bootstrap(
                        path,
                        base_dir,
                        provider_context,
                        validated_config,
                        bootstrap_unresolved,
                    )?;
                    let parsed = validated_config.resources().parsed();

                    let backend_resource_type = backend
                        .resource_type()
                        .ok_or("Backend does not specify a resource type")?;
                    let bucket_resource_name = parsed
                        .find_resource_by_attr(backend_resource_type, "bucket", &bucket_name)
                        .and_then(|r| r.id.identity().cloned())
                        .ok_or_else(|| {
                            format!(
                                "Auto-injected state bucket resource '{}' not found after re-parse",
                                bucket_name
                            )
                        })?;
                    let initial_state = StateFile::with_managed_state_bucket(
                        backend_provider_name,
                        backend_resource_type,
                        bucket_resource_name,
                        AppliedIdentifier::new(&bucket_name).map_err(|source| {
                            AppError::Config(format!(
                                "State bucket has an invalid provider identifier: {source}"
                            ))
                        })?,
                    )
                    .map_err(|source| {
                        AppError::Config(format!("Failed to encode initial state bucket: {source}"))
                    })?;
                    backend
                        .write_state(&initial_state)
                        .await
                        .map_err(AppError::Backend)?;
                    println!(
                        "  {} Registered state bucket as protected resource",
                        "✓".green(color_mode)
                    );
                } else {
                    return Err(AppError::Config(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    )));
                }
            }
        }
    }

    // Acquire lock (unless --lock=false)
    let state_lock = if lock {
        println!("{}", "Acquiring state lock...".cyan(color_mode));
        let acquired = backend
            .acquire_lock("apply")
            .await
            .map_err(|error| map_lock_error(error, path))?;
        println!("  {} Lock acquired", "✓".green(color_mode));
        StateLock::held(acquired)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow(color_mode)
                .bold(color_mode)
        );
        StateLock::disabled_by_cli_flag()
    };

    // All code after lock acquisition observes the shared cancellation token.
    // A lock that survives execution is released after cancellation; proven
    // heartbeat loss returns without a capability that could be released.
    let outcome = run_apply_locked_owned(
        validated_config,
        auto_approve,
        Arc::clone(&backend),
        state_lock,
        path,
        provider_context,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await;
    let timing = finish_lock_command(color_mode, backend.as_ref(), outcome, &cancel).await?;
    if let Some(elapsed) = timing {
        println!("{}", format_total_apply_line(elapsed));
    }

    Ok(())
}

fn reload_apply_configuration_after_bootstrap(
    path: &Path,
    base_dir: &Path,
    provider_context: &ProviderContext,
    validated_config: ValidatedConfig,
    unresolved: carina_core::config_loader::UnresolvedConfigSnapshot,
) -> Result<ValidatedConfig, AppError> {
    let reloaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let duplicate_declarations = reloaded.duplicate_declarations;
    validated_config.revalidate_config_pair(
        reloaded.parsed,
        unresolved,
        base_dir,
        true,
        &duplicate_declarations,
    )
}

async fn load_apply_provider_inputs<E>(
    runtime: &crate::runtime::CommandRuntime,
    parsed: &carina_core::parser::ExpandedFile<E>,
    base_dir: &Path,
    provider_context: &ProviderContext,
) -> Result<ResolvedProviderInputs, AppError> {
    load_and_resolve_provider_configs(
        runtime,
        parsed,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::All,
    )
    .await
}

async fn create_state_bucket_bootstrap_provider<R>(
    validated_config: &ValidatedConfig<R>,
    provider_configs: &ResolvedProviderConfigs,
    provider_name: &str,
) -> Result<
    (
        ProviderRouter,
        Box<dyn carina_core::provider::ProviderNormalizer>,
    ),
    AppError,
> {
    let ctx = validated_config.context();
    let factory = provider_mod::find_factory(ctx.factories(), provider_name)
        .ok_or_else(|| format!("No provider factory found for '{provider_name}'"))?;
    let config = prepare_default_provider_ready_config(factory, provider_name, provider_configs)?;
    let provider = factory.create_initialized_provider(None, &config).await?;
    let normalizer = factory.create_normalizer(None, &config).await?;
    let mut router = ProviderRouter::new();
    router.add_provider(provider_name.to_string(), provider);
    Ok((router, normalizer))
}

type CheckedApplyExports = carina_core::validation::ExportsCheckedPreApply<
    carina_core::parser::ExpandedInferredExportParam,
>;

enum SourceApplyAfterHeartbeat {
    Done(Option<Duration>),
    PersistExports(Box<SourceApplyPersistExports>),
    Executed(Box<SourceApplyExecuted>),
}

struct SourceApplyPersistExports {
    validated_config: ValidatedConfig<
        crate::wiring::ExpandedCanonicalResources<carina_core::parser::ExpandedInferredExportParam>,
    >,
    state_file: Option<StateFile>,
    resources: Vec<Resource>,
    compositions: Vec<carina_core::resource::LiveComposition>,
    checked_exports: CheckedApplyExports,
    wait_aliases: Vec<WaitAliasSpec>,
    managed_current: HashMap<ResourceId, Current>,
    data_source_values: DataSourceValues,
    directive_change_count: usize,
    export_change_count: usize,
}

struct SourceApplyExecuted {
    result: ExecutionResult,
    cancelled: bool,
    resources_finished: Instant,
    apply_phase_started: Instant,
    state_base: FinalStateBase,
    state_save_inputs: Arc<ApplyStateSaveInputs>,
    data_source_values: DataSourceValues,
    checked_exports: CheckedApplyExports,
    wait_aliases: Vec<WaitAliasSpec>,
    pre_resolve_compositions: Vec<carina_core::resource::LiveComposition>,
    provider_context: LoadedProviderContext,
    cancel: ShutdownToken,
}

enum SavedPlanApplyAfterHeartbeat {
    Done(Option<Duration>),
    PersistStateOnly(Box<SavedPlanPersistStateOnly>),
    Executed(Box<SavedPlanApplyExecuted>),
}

struct SavedPlanPersistStateOnly {
    state: StateFile,
    directive_change_count: usize,
}

struct SavedPlanApplyExecuted {
    result: ExecutionResult<SavedModuleSource>,
    cancelled: bool,
    resources_finished: Instant,
    apply_phase_started: Instant,
    state_base: FinalStateBase,
    state_save_inputs: Arc<ApplyStateSaveInputs<SavedModuleSource>>,
    wait_aliases: Vec<WaitAliasSpec>,
    cancel: ShutdownToken,
}

#[allow(clippy::too_many_arguments)]
async fn run_apply_locked_owned(
    mut validated_config: ValidatedConfig,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    project_dir: &Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> LockCommandOutcome<Result<Option<Duration>, AppError>> {
    let runtime = validated_config.context().runtime().clone();
    let color_mode = validated_config.context().runtime().color_mode();
    // Read current state from backend. carina#3315: if `check_and_migrate`
    // lifted an older on-disk schema in memory, persist the upgrade
    // under the current lock before any short-circuit path can return
    // — otherwise the carina#3283 warning text ("Disk state will be
    // rewritten during the next apply or state refresh of that project.")
    // becomes a lie.
    let mut state_file = match load_state_persist_if_migrated(backend.as_ref(), lock.as_ref()).await
    {
        Ok(state) => state,
        Err(error) => {
            return LockCommandOutcome::Kept {
                result: Err(error),
                lock,
            };
        }
    };
    if let Err(error) = prune_legacy_data_source_rows_persist_if_changed(
        backend.as_ref(),
        lock.as_ref(),
        &mut state_file,
        &validated_config.resources().parsed().data_sources,
    )
    .await
    {
        return LockCommandOutcome::Kept {
            result: Err(error),
            lock,
        };
    }
    let heartbeat_outcome = with_lock_heartbeat(
        Arc::clone(&backend),
        lock,
        cancel,
        async |cancel, mid_run_writer| {
            if let Some(state) = state_file.as_ref() {
                check_legacy_name_overrides(
                    state,
                    accept_legacy_name_overrides,
                    ApplyTarget::Project(project_dir),
                )?;
            }

            let crate::wiring::StateBlockResolution {
                claims: state_block_claims,
                targets: resolved_state_block_targets,
            } = validated_config.reconcile_identities_with_state(&state_file);
            let (ctx, canonical_resources) = validated_config.context_and_resources();
            let parsed = canonical_resources.parsed();
            let provider_inputs = load_apply_provider_inputs(
                validated_config.context().runtime(),
                parsed,
                project_dir,
                provider_context,
            )
            .await?;
            let provider_configs = provider_inputs.configs();
            let raw_provider_configs = provider_configs.as_provider_configs();
            let remote_bindings = provider_inputs.remote_bindings();
            // Select appropriate Provider based on configuration
            let provider =
                get_provider_with_ctx(ctx, provider_configs, ProgressSink::Human).await?;

            // carina#3132: `sorted_resources` is `mut` because deferred-for
            // expansion now runs post-refresh (after phase-2, below) via the
            // shared `expand_same_config_deferred_for` and re-sorts the
            // augmented set in place — same timing/view as the plan path so
            // plan and apply cannot diverge on same-config read iterables.
            // Until then it is the pre-expansion set; the loop's iterable
            // source (e.g. `let cert`) is a normal top-level resource already
            // here and refreshed by phase 1, only the loop's generated children
            // are added later.
            // carina#3181: `parsed.resources` is managed-only and
            // `parsed.data_sources` holds the `read`-keyword resources. Only
            // managed resources are dependency-sorted; data sources are
            // refreshed in a later phase against the populated `managed_current`.
            let expansion_prelude = crate::wiring::DeferredForExpansionPrelude::new(
                canonical_resources,
                &state_file,
                ctx.schemas(),
            )?
            .detect_moves(
                ctx,
                &provider,
                &parsed.data_sources,
                raw_provider_configs,
                &state_block_claims,
            )?;
            let data_sources: Vec<carina_core::resource::DataSource> = parsed.data_sources.clone();
            // Read states for all resources using identifier from state
            // An address with no state row is absent. A persisted row without
            // an identifier stays present and invalid until planning reports it.
            // Skip composition resources (module attribute containers) — they have no infrastructure.
            let progress_backend = ProgressBackend::new(color_mode, ProgressSink::Human);
            let refresh_phase = RefreshPhase::start(&progress_backend);
            let provider_ref = &provider;
            let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();
            // Pre-build dependency_bindings from state file so we can restore them
            // after refresh. Provider.read() doesn't know about this metadata (#1565).
            let saved_dep_bindings = expansion_prelude.saved_dependency_bindings();

            // Phase 1: refresh managed resources in parallel. carina#3181:
            // `sorted_resources` is managed-only — data sources are refreshed in
            // phase 2 below.
            let persisted_state = state_file.as_ref();
            let schemas = ctx.schemas();
            expansion_prelude
                .refresh_managed_resource_set(
                    ProjectedPhase1RefreshInputs {
                        provider: provider_ref,
                        refresh_phase: &refresh_phase,
                        schemas,
                    },
                    &mut managed_current,
                )
                .await?;

            // Refresh orphaned resources (#844, #1685). Must run before the
            // automatic move transfer below so old-address entries are present
            // for the shared resolved-move path to transfer.
            let mut orphans = Orphans::new();
            // Ids the orphan pass already live-read this run. A for-loop child
            // applied previously is in state but not yet a desired resource at
            // orphan time (expansion is post-refresh); the orphan pass live-reads
            // it under the same state name the child resolves to.
            // `expand_same_config_deferred_for` excludes these from the
            // post-expansion child refresh so the same address is not read twice
            // (carina#3145). Mirrors the plan path in `wiring/mod.rs`.
            let mut orphan_refreshed_ids: HashSet<ResourceId> = HashSet::new();
            if persisted_state.is_some() {
                let loaded_orphans = expansion_prelude.loaded_orphans(std::iter::empty());
                let orphan_results: Vec<
                    Result<(ResourceId, carina_core::differ::Orphan), AppError>,
                > = stream::iter(loaded_orphans)
                    .map(|(id, loaded)| async move {
                        let refreshed = refresh_loaded_orphan_with_retry(
                            provider_ref,
                            loaded,
                            persisted_state,
                            schemas,
                        )
                        .await
                        .map_err(AppError::Provider)?;
                        Ok((id, refreshed))
                    })
                    .buffer_unordered(5)
                    .collect()
                    .await;
                let mut refreshed_orphans = carina_state::LoadedOrphans::default();
                for result in orphan_results {
                    let (id, loaded) = result?;
                    let exists = match loaded.current() {
                        Current::Absent(_state) => false,
                        Current::Present(_present) => true,
                        Current::CreateInFlight(_pending) => true,
                        Current::CreateOutcomeUnknown(_unknown) => true,
                        Current::MissingIdentifier(_state) => true,
                    };
                    if exists {
                        orphan_refreshed_ids.insert(id.clone());
                    }
                    refreshed_orphans.insert(loaded);
                }
                orphans = refreshed_orphans.into_plan_orphans();
            }

            // Hydrate, transfer explicit and detected anonymous address moves
            // (#1685), then run phase 2 against the consolidated state.
            crate::wiring::hydrate_prior_managed_states(
                &mut managed_current,
                &mut orphans,
                &provider,
                expansion_prelude.saved_attrs(),
                state_file.as_ref(),
            )
            .await?;
            let reconciled_expansion = expansion_prelude
                .bind_projected_phase1_state(managed_current, orphans, orphan_refreshed_ids)
                .reconcile_addresses(
                    color_mode,
                    parsed,
                    crate::wiring::DeferredForWarningPolicy::Print,
                )?;

            // Phase 2 uses the same addressed managed/orphan view as plan and
            // state refresh and produces one outcome per query.
            let phase2 = reconciled_expansion
                .read_data_sources_phase2(AddressedDataSourceReadInputs {
                    provider: &provider,
                    refresh_phase: Some(&refresh_phase),
                    compositions: &parsed.compositions,
                    data_sources: &data_sources,
                    remote_bindings,
                    schemas: ctx.schemas(),
                    saved_dep_bindings: &saved_dep_bindings,
                    factories: ctx.factories(),
                    cancel: Some(&cancel),
                })
                .await?;
            let refresh_module_constraint_errors = phase2
                .module_constraint_error
                .and_then(module_constraint_plan_errors);
            let data_source_values = phase2.values;
            let prepared_expansion = reconciled_expansion.seal(data_source_values);

            // carina#3132: expand same-config deferred-for loops post-refresh
            // via the *same* `expand_same_config_deferred_for` the plan path
            // calls (one resolution timing; plan/apply parity). Then
            // targeted-refresh the materialized children through the same
            // `refresh_resource_set` helper phase 1 used, so a re-apply after
            // they were created sees their live state instead of a phantom
            // Create. The #1844 sort constraint (expanded resources must be in
            // the sorted/refreshed/diffed set) is honored by the re-sort
            // inside `expand_same_config_deferred_for`.
            //
            // carina#3278: the expand → child-refresh → hydrate(2nd) → lift
            // quartet runs through
            // `expand_validated_canonical_refresh_and_lift_states` so the apply
            // and refresh paths cannot diverge on these phases. The transition
            // consumes the prepared addressed state into the post-expansion
            // plan view, so leaving a phase out becomes a compile error.
            let validated_expansion =
                crate::wiring::expand_validated_canonical_refresh_and_lift_states(
                    validated_config,
                    crate::wiring::ValidatedCanonicalExpandRefreshAndLiftInputs {
                        provider: &provider,
                        comparison_provider: &provider,
                        expansion: prepared_expansion,
                        warning_policy: crate::wiring::DeferredForWarningPolicy::Print,
                        remote_bindings,
                        state_file: &state_file,
                        saved_dep_bindings: &HashMap::new(),
                        refresh_phase: &refresh_phase,
                    },
                )
                .await?;
            drop(refresh_phase);
            let post_expansion = validated_expansion
                .finish_post_expansion_plan::<crate::wiring::ApplyMode>(
                    crate::wiring::PostExpansionPlanEnvironment {
                        data_sources: &data_sources,
                        provider: &provider,
                        normalizer: &provider,
                        provider_configs: raw_provider_configs,
                        remote_bindings,
                        state_file: &state_file,
                        state_block_claims: &state_block_claims,
                        resolved_state_block_targets: &resolved_state_block_targets,
                        preparation_errors: refresh_module_constraint_errors,
                    },
                )
                .await?;
            let crate::wiring::PostExpansionPlan {
                artifacts,
                resources,
            } = match post_expansion {
                crate::wiring::PostExpansionOutcome::Planned(planned) => planned,
                crate::wiring::PostExpansionOutcome::Diagnostics(diagnostics) => {
                    return Err(diagnostics.render_and_abort(color_mode, project_dir));
                }
            };
            let crate::wiring::PostExpansionArtifacts {
                expanded_resources: validated_expansion,
                residual_deferred_for,
                plan,
                compositions: pre_resolve_compositions,
                data_sources: _,
                mut managed_current,
                mut data_source_values,
                moved_origins,
                prev_explicit,
                wait_aliases,
            } = artifacts;
            render_plan_errors_and_abort(color_mode, &plan, project_dir)?;
            #[cfg(any(test, feature = "test-utils"))]
            capture_live_apply_plan_for_test(&runtime, &plan)?;
            let crate::wiring::PostExpansionApplyResources {
                resolved: resolved_resources,
                paired_unresolved: paired_unresolved_resources,
                unresolved_by_resolved_id,
                bindings,
            } = resources;
            let mut bindings = bindings;
            let (ctx, expanded_canonical_resources) = validated_expansion.context_and_resources();
            let parsed = expanded_canonical_resources.parsed();
            let executable_data_source_values = data_source_values
                .clone()
                .into_executable()
                .map_err(|error| AppError::Config(error.to_string()))?;

            if can_use_export_only_fast_path(&plan, &executable_data_source_values) {
                // No mutating effects — the plan only holds `Read` (data-source
                // reads) or `Wait` effects, or is entirely empty. Either way no
                // resource apply pipeline needs to run; route through the
                // export-only fast path. Without this, a plan whose only
                // difference from state is an export expression that reads a
                // data source (a `Read` effect lands in the plan, but no
                // mutation does) silently falls through into the
                // resource-apply pipeline and the `Persisting N export
                // change(s) to state.` banner never prints (carina#3270).
                finalize_module_constraints(&pre_resolve_compositions, &bindings)?;
                let checked_exports_pre_apply =
                    crate::commands::plan::check_exports_pre_apply(CheckExportsPreApplyInput {
                        export_params: &parsed.export_params,
                        resources: &paired_unresolved_resources,
                        compositions: &parsed.compositions,
                        managed_current: &managed_current,
                        data_source_values: &data_source_values,
                        state_file: state_file.as_ref(),
                        plan: &plan,
                        schemas: ctx.schemas(),
                        wait_aliases: &wait_aliases,
                        provider_context: ctx.provider_context(),
                    })
                    .map_err(AppError::Validation)?;
                let empty_exports = HashMap::new();
                let current_exports = state_file
                    .as_ref()
                    .map(|s| &s.exports)
                    .unwrap_or(&empty_exports);
                let export_changes = crate::commands::plan::compute_export_diffs(
                    &checked_exports_pre_apply,
                    current_exports,
                );
                let directive_changes =
                    persisted_directive_changes(state_file.as_ref(), &resolved_resources, &plan);
                let directive_change_count = directive_changes.len();

                if export_changes.is_empty() && directive_change_count == 0 {
                    println!("{}", "No changes needed.".green(color_mode));
                    return Ok(SourceApplyAfterHeartbeat::Done(None));
                }

                let historical_display =
                    carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
                let summary = PlanChangeSummary::new(
                    &plan,
                    &export_changes,
                    &residual_deferred_for,
                    &directive_changes,
                );
                print_plan(
                    color_mode,
                    &plan,
                    &summary,
                    DetailLevel::Full,
                    &historical_display,
                    Some(ctx.schemas()),
                    &HashMap::new(),
                    None,
                    None,
                    &provider,
                )?;
                // This fast path has no managed-resource mutations, so it
                // cannot contain the Delete + Create pair required for a
                // moved-block hint.

                let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                if confirm_apply(&runtime, stdin, cancel.clone(), auto_approve).await?
                    == ApplyConfirmation::Cancelled
                {
                    return Ok(SourceApplyAfterHeartbeat::Done(None));
                }

                if !export_changes.is_empty() {
                    println!(
                        "{}",
                        format!(
                            "Persisting {} export change(s) to state.",
                            export_changes.len()
                        )
                        .cyan(color_mode)
                    );
                }
                if directive_change_count > 0 {
                    println!(
                        "{}",
                        format!(
                            "Persisting {} resource directive change(s) to state.",
                            directive_change_count
                        )
                        .cyan(color_mode)
                    );
                }
                let state_file = if directive_change_count > 0 {
                    let mut state = state_file.unwrap_or_default();
                    apply_persisted_directive_changes(
                        &mut state,
                        &resolved_resources,
                        &directive_changes,
                    )?;
                    Some(state)
                } else {
                    state_file
                };
                // No-op apply path: compositions were never resolved (no head-of-
                // pipeline pass ran). carina#3181: compositions live in
                // `parsed.compositions` and still carry the authored
                // `ResourceRef` snapshots — the shape post-apply export resolution expects
                // from the post-apply path.
                let pre_resolve_compositions_noop: Vec<carina_core::resource::LiveComposition> =
                    parsed.compositions.clone();
                return Ok(SourceApplyAfterHeartbeat::PersistExports(Box::new(
                    SourceApplyPersistExports {
                        validated_config: validated_expansion,
                        state_file,
                        resources: paired_unresolved_resources,
                        compositions: pre_resolve_compositions_noop,
                        checked_exports: checked_exports_pre_apply,
                        wait_aliases,
                        managed_current,
                        data_source_values,
                        directive_change_count,
                        export_change_count: export_changes.len(),
                    },
                )));
            }

            // Build display-safe historical attributes from current and persisted state.
            let historical_display =
                collect_historical_display_attributes(&plan, &managed_current, state_file.as_ref());

            let checked_exports_pre_apply =
                crate::commands::plan::check_exports_pre_apply(CheckExportsPreApplyInput {
                    export_params: &parsed.export_params,
                    resources: &paired_unresolved_resources,
                    compositions: &parsed.compositions,
                    managed_current: &managed_current,
                    data_source_values: &data_source_values,
                    state_file: state_file.as_ref(),
                    plan: &plan,
                    schemas: ctx.schemas(),
                    wait_aliases: &wait_aliases,
                    provider_context: ctx.provider_context(),
                })
                .map_err(AppError::Validation)?;
            let current_exports = state_file
                .as_ref()
                .map(|s| s.exports.clone())
                .unwrap_or_default();
            let export_changes = crate::commands::plan::compute_export_diffs(
                &checked_exports_pre_apply,
                &current_exports,
            );
            let directive_changes =
                persisted_directive_changes(state_file.as_ref(), &resolved_resources, &plan);
            let summary = PlanChangeSummary::new(
                &plan,
                &export_changes,
                &residual_deferred_for,
                &directive_changes,
            );
            print_plan(
                color_mode,
                &plan,
                &summary,
                DetailLevel::Full,
                &historical_display,
                Some(ctx.schemas()),
                &moved_origins,
                Some(&prev_explicit),
                None,
                &provider,
            )?;
            let moved_hints = crate::moved_hint::detect_moved_block_hints(
                &plan,
                &managed_current,
                ctx.schemas(),
                &provider,
            )?;
            moved_hints.eprint(color_mode);

            let stdin = tokio::io::BufReader::new(tokio::io::stdin());
            if confirm_apply(&runtime, stdin, cancel.clone(), auto_approve).await?
                == ApplyConfirmation::Cancelled
            {
                return Ok(SourceApplyAfterHeartbeat::Done(None));
            }

            let apply_phase_started = Instant::now();
            println!(
                "{}",
                "Applying changes...".cyan(color_mode).bold(color_mode)
            );
            println!();

            // Preserve the old execution boundary for the fallible unresolved
            // map while deriving it from the exact resource view used by the
            // shared plan seam.
            let unresolved_resources = unresolved_by_resolved_id?;
            // `provider` is a `ProviderRouter`, which impls both `Provider` and
            // `ProviderNormalizer`; the same object is passed in both positions
            // so apply re-normalizes with exactly the plan-time normalizer
            // (carina#3060). They must stay the same object.
            let observer = observer_factory(color_mode, &plan, project_dir);
            let provider = Arc::new(provider);
            let state_save_inputs = Arc::new(ApplyStateSaveInputs::new(
                resolved_resources.clone(),
                plan.clone(),
                ctx.schemas().clone(),
                None,
                provider.as_ref(),
            )?);
            let saver =
                start_apply_state_saver(state_file, mid_run_writer, Arc::clone(&state_save_inputs));
            let publisher = saver.publisher();
            let checkpoint_sink = ExecutionCheckpointSink::new(move |snapshot| {
                publisher.send_replace(Some(snapshot));
            });
            // Remove, Move, and keyed-list release need no provider call.
            // Report them now; every checkpoint's state writeback materializes
            // them from the plan.
            let state_only_success_count = execute_state_only_effects(color_mode, &plan);
            let outcome = execute_effects_with_observer(
                &plan,
                provider.as_ref(),
                provider.as_ref(),
                raw_provider_configs,
                ctx.factories(),
                ctx.schemas(),
                &mut bindings,
                &mut managed_current,
                executable_data_source_values,
                &unresolved_resources,
                &data_sources,
                &pre_resolve_compositions,
                state_only_success_count,
                checkpoint_sink.clone(),
                cancel.clone(),
                parallelism,
                observer.as_ref(),
            )
            .await;
            let (mut result, cancelled) = split_execution_outcome(outcome);
            data_source_values
                .merge_read_states(&result.data_source_states)
                .map_err(|error| AppError::Config(error.to_string()))?;

            // Execute import effects: read imported resources from the provider
            execute_import_effects(
                color_mode,
                &plan,
                provider.as_ref(),
                &mut result,
                &cancel,
                &checkpoint_sink,
                observer.as_ref(),
            )
            .await;
            let state_base = saver.stop(&cancel).await;
            let resources_finished = Instant::now();

            Ok::<_, AppError>(SourceApplyAfterHeartbeat::Executed(Box::new(
                SourceApplyExecuted {
                    result,
                    cancelled,
                    resources_finished,
                    apply_phase_started,
                    state_base,
                    state_save_inputs,
                    data_source_values,
                    checked_exports: checked_exports_pre_apply,
                    wait_aliases,
                    pre_resolve_compositions,
                    provider_context: ctx.provider_context().clone(),
                    cancel,
                },
            )))
        },
    )
    .await;
    let (heartbeat_result, lock) = match heartbeat_outcome {
        HeartbeatOutcome::Kept { result, lock } => (result, lock),
        HeartbeatOutcome::Lost {
            result: _result,
            lost,
        } => {
            return LockCommandOutcome::Lost(AppError::StateLockLost {
                error: lost.into_error(),
            });
        }
    };

    let op_result = async {
        match heartbeat_result? {
            SourceApplyAfterHeartbeat::Done(result) => Ok(result),
            SourceApplyAfterHeartbeat::PersistExports(context) => {
                let SourceApplyPersistExports {
                    validated_config,
                    state_file,
                    resources,
                    compositions,
                    checked_exports,
                    wait_aliases,
                    managed_current,
                    data_source_values,
                    directive_change_count,
                    export_change_count,
                } = *context;
                if export_change_count == 0 {
                    persist_directives_only(
                        color_mode,
                        backend.as_ref(),
                        lock.as_ref(),
                        state_file.unwrap_or_default(),
                        directive_change_count,
                    )
                    .await?;
                } else {
                    persist_exports_only(
                        color_mode,
                        backend.as_ref(),
                        lock.as_ref(),
                        state_file,
                        &resources,
                        &data_source_values,
                        &compositions,
                        &checked_exports,
                        validated_config.context().schemas(),
                        &wait_aliases,
                        &managed_current,
                        validated_config.context().provider_context(),
                    )
                    .await?;
                    if directive_change_count > 0 {
                        println!(
                            "  {} {} resource directive change(s) persisted",
                            "✓".green(color_mode),
                            directive_change_count
                        );
                    }
                }
                Ok(None)
            }
            SourceApplyAfterHeartbeat::Executed(context) => {
                let SourceApplyExecuted {
                    result,
                    cancelled,
                    resources_finished,
                    apply_phase_started,
                    state_base,
                    state_save_inputs,
                    data_source_values,
                    checked_exports,
                    wait_aliases,
                    pre_resolve_compositions,
                    provider_context,
                    cancel,
                } = *context;
                let export_resources: Vec<Resource> = state_save_inputs
                    .sorted_resources()
                    .iter()
                    .map(|resource| resource.as_resource().clone())
                    .collect();
                // Use override-aware resources (post-default_tags merge, post-canonicalize)
                // for state writeback so the per-resource `explicit` tree includes
                // provider-level default tags. Otherwise the next plan projects the
                // tags out of `current` and surfaces a spurious `tags: (none) → ...`
                // diff (refs awscc#206).
                let skipped_exports = finalize_after_execute(
                    |persistence| {
                        finalize_apply(
                            color_mode,
                            FinalizeApplyInput {
                                result: &result,
                                state_base,
                                state_save_inputs,
                                export_resolution_inputs: Some(ExportResolutionInputs {
                                    export_params: &checked_exports,
                                    data_source_values: &data_source_values,
                                    resources: &export_resources,
                                    provider_context: &provider_context,
                                }),
                                backend: backend.as_ref(),
                                lock: lock.as_ref(),
                                wait_aliases: &wait_aliases,
                                pre_resolve_compositions: &pre_resolve_compositions,
                            },
                            persistence,
                        )
                    },
                    cancelled,
                    &cancel,
                )
                .await?;

                finish_apply(
                    color_mode,
                    &result,
                    skipped_exports,
                    resources_finished.duration_since(apply_phase_started),
                )
            }
        }
    }
    .await;

    LockCommandOutcome::Kept {
        result: op_result,
        lock,
    }
}

#[cfg(any(test, feature = "test-utils"))]
fn capture_live_apply_plan_for_test(
    runtime: &crate::runtime::CommandRuntime,
    plan: &Plan,
) -> Result<(), AppError> {
    let Some(path) = &runtime.hooks().capture_live_apply_plan else {
        return Ok(());
    };
    let encoded = serde_json::to_vec_pretty(plan)
        .map_err(|error| AppError::Config(format!("serialize live apply plan: {error}")))?;
    std::fs::write(path, encoded).map_err(|error| {
        AppError::Config(format!(
            "write live apply plan to {}: {error}",
            path.display()
        ))
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_apply_locked(
    validated_config: ValidatedConfig,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    project_dir: &Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> Result<Option<Duration>, AppError> {
    run_apply_locked_canonical(
        validated_config,
        auto_approve,
        backend,
        lock,
        project_dir,
        provider_context,
        cancel,
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_apply_locked_canonical(
    validated_config: ValidatedConfig,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    project_dir: &Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> Result<Option<Duration>, AppError> {
    let color_mode = validated_config.context().runtime().color_mode();
    let outcome = run_apply_locked_owned(
        validated_config,
        auto_approve,
        Arc::clone(&backend),
        lock,
        project_dir,
        provider_context,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await;
    finish_lock_command(color_mode, backend.as_ref(), outcome, &cancel).await
}

fn ensure_saved_plan_backend_matches_current(
    plan_backend_config: Option<&carina_core::parser::ExpandedBackendConfig>,
    current_backend_config: Option<&carina_core::parser::ExpandedBackendConfig>,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    let planned = BackendLock::for_config(plan_backend_config).map_err(AppError::Backend)?;
    let current = BackendLock::for_config(current_backend_config).map_err(AppError::Backend)?;

    if planned == current {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "Saved plan backend does not match the current `backend.crn`.\n\
             The plan file recorded one backend, but the project now declares another.\n\
             Re-run `{}` to produce a fresh saved plan before applying.\n{}",
            apply_target.replan_command(),
            planned.describe_diff(&current)
        )))
    }
}

trait SavedPlanResourceLike {
    fn id(&self) -> &ResourceId;
    fn binding(&self) -> Option<&str>;
    fn attributes(&self) -> &indexmap::IndexMap<String, Value>;
}

impl<S> SavedPlanResourceLike for carina_core::resource::Resource<S> {
    fn id(&self) -> &ResourceId {
        &self.id
    }

    fn binding(&self) -> Option<&str> {
        self.binding.as_ref().map(|binding| binding.as_str())
    }

    fn attributes(&self) -> &indexmap::IndexMap<String, Value> {
        &self.attributes
    }
}

impl<S> SavedPlanResourceLike for carina_core::resource::DataSource<S> {
    fn id(&self) -> &ResourceId {
        &self.id
    }

    fn binding(&self) -> Option<&str> {
        self.binding.as_ref().map(|binding| binding.as_str())
    }

    fn attributes(&self) -> &indexmap::IndexMap<String, Value> {
        &self.attributes
    }
}

fn rehydrate_saved_resource_like<T, U, Rehydrate>(
    saved: &mut [T],
    candidates: &HashMap<ResourceId, U>,
    apply_target: ApplyTarget<'_>,
    rehydrate: Rehydrate,
) -> Result<(), AppError>
where
    T: SavedPlanResourceLike,
    Rehydrate: Fn(&T, &U) -> Result<T, carina_core::value::SecretRehydrationSiteError>,
{
    for entry in saved {
        if !entry
            .attributes()
            .values()
            .any(carina_core::value::contains_redacted_secret)
        {
            continue;
        }
        let candidate = candidates
            .get(entry.id())
            .ok_or_else(|| saved_plan_secret_shape_error(entry.id().to_string(), apply_target))?;
        *entry = rehydrate(entry, candidate)
            .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
    }
    Ok(())
}

fn matching_resource_like<'a, T, U>(saved: &T, fresh: &'a [U]) -> Option<&'a U>
where
    T: SavedPlanResourceLike,
    U: SavedPlanResourceLike,
{
    fresh
        .iter()
        .find(|candidate| candidate.id() == saved.id())
        .or_else(|| {
            let binding = saved.binding()?;
            fresh.iter().find(|candidate| {
                candidate.binding() == Some(binding)
                    && candidate.id().provider == saved.id().provider
                    && candidate.id().resource_type == saved.id().resource_type
                    && candidate.id().provider_instance == saved.id().provider_instance
            })
        })
}

fn import_identifiers_from_current_config<S>(
    plan: &carina_core::plan::Plan<S>,
    parsed: &carina_core::parser::ExpandedInferredFile,
) -> HashMap<ResourceId, Value> {
    let mut identifiers = HashMap::new();
    let bindings = bootstrap_resolved_bindings(parsed);
    let unresolved_upstream_bindings = HashSet::new();
    for effect in plan.effects() {
        let carina_core::effect::Effect::Import { id, .. } = effect else {
            continue;
        };
        if let Some(value) = parsed.state_blocks.iter().find_map(|block| match block {
            carina_core::parser::StateBlock::Import {
                to,
                to_span: _,
                id: value,
            } if crate::wiring::resolve_import_target(to, &parsed.resources) == *id => {
                Some(carina_core::resolver::resolve_value_for_plan(
                    value,
                    &bindings,
                    &unresolved_upstream_bindings,
                ))
            }
            _ => None,
        }) {
            identifiers.insert(id.as_inner().clone(), value);
        }
    }
    identifiers
}

/// Reproduce the state-aware desired-side preparation that plan runs before
/// persisting secret placeholders. The supplied wiring has already passed the
/// saved-plan schema check; only then may this function build normalizers.
struct PreparedSavedPlanSecretCandidates {
    candidates: SavedPlanSecretCandidates,
    provider: ProviderRouter,
}

async fn prepare_saved_plan_secret_candidates(
    mut validated_config: ValidatedConfig,
    pending_plan: &SchemaCheckedPendingPlanFile,
    state_file: &Option<StateFile>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    apply_target: ApplyTarget<'_>,
) -> Result<PreparedSavedPlanSecretCandidates, AppError> {
    let color_mode = validated_config.context().runtime().color_mode();
    let mut candidate_state = state_file.clone();
    let PreparedPlanIdentityData {
        provider_inputs,
        state_blocks,
    } = prepare_plan_identity_and_provider_inputs(
        &mut validated_config,
        &mut candidate_state,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
    )
    .await?;

    // Reuse the exact I/O-free prelude the plan path uses. `candidate_state`
    // is the identity-reconciled state view produced above; the current-state
    // values come from the saved plan's plan-time snapshots, so verification
    // neither performs provider reads nor observes apply-time drift.
    let wiring = validated_config.context();
    let canonical_resources = validated_config.resources();
    let parsed = canonical_resources.parsed();
    let provider =
        create_providers_from_configs_with_ctx(wiring, provider_inputs.configs()).await?;
    let expansion_prelude = crate::wiring::DeferredForExpansionPrelude::new(
        canonical_resources,
        &candidate_state,
        wiring.schemas(),
    )?
    .detect_moves(
        wiring,
        &provider,
        &parsed.data_sources,
        provider_inputs.configs().as_provider_configs(),
        &state_blocks.claims,
    )?;
    let replay_moves = expansion_prelude.saved_plan_replay_moves(parsed);
    let prior_states = pending_plan.expansion_prior_states(
        expansion_prelude.sorted_resources(),
        &replay_moves,
        &candidate_state,
    )?;
    let data_source_values = pending_plan.data_source_values(&parsed.data_sources)?;
    pending_plan.verify_provider_scopes(&provider, apply_target)?;
    let prepared_expansion = expansion_prelude
        .bind_projected_phase1_state(
            prior_states.managed,
            prior_states.orphans,
            prior_states.already_refreshed,
        )
        .reconcile_addresses(
            color_mode,
            parsed,
            crate::wiring::DeferredForWarningPolicy::Suppress,
        )?
        .seal(data_source_values);
    let (mut validated_expansion, _expansion) = validated_config.expand_deferred_for(
        &prepared_expansion,
        provider_inputs.remote_bindings(),
        crate::wiring::DeferredForWarningPolicy::Suppress,
        &provider,
    )?;

    let normalizers = create_normalizers_from_configs_with_ctx(
        validated_expansion.context(),
        provider_inputs.configs(),
    )
    .await?;
    validated_expansion
        .prepare_desired_side_for_plan(
            provider_inputs.configs().as_provider_configs(),
            &normalizers,
        )
        .await
        .map_err(AppError::Provider)?;

    let candidates =
        SavedPlanSecretCandidates::from_expansion(validated_expansion, provider_inputs);

    Ok(PreparedSavedPlanSecretCandidates {
        candidates,
        provider,
    })
}

fn rehydrate_desired_plan_file(
    plan_file: &mut ReloadedPlanFile,
    candidates: &SavedPlanSecretCandidates,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    rehydrate_desired_plan_file_from_parts(
        plan_file,
        candidates.parsed(),
        candidates.provider_inputs(),
        apply_target,
    )
}

fn rehydrate_desired_plan_file_from_parts(
    plan_file: &mut ReloadedPlanFile,
    parsed: &carina_core::parser::ExpandedInferredFile,
    provider_inputs: &ResolvedProviderInputs,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    let mut resource_candidates = HashMap::new();
    for saved in plan_file.sorted_resources() {
        if let Some(fresh) = matching_resource_like(saved, &parsed.resources) {
            resource_candidates.insert(saved.id.clone(), fresh.clone());
        }
    }
    let mut data_source_candidates = HashMap::new();
    for saved in plan_file.data_sources() {
        if let Some(fresh) = matching_resource_like(saved, &parsed.data_sources) {
            data_source_candidates.insert(saved.id.clone(), fresh.clone());
        }
    }
    let import_identifiers = import_identifiers_from_current_config(plan_file.plan(), parsed);

    let rehydrated_plan = carina_core::value::rehydrate_plan_desired_secrets(
        plan_file.plan(),
        &resource_candidates,
        &data_source_candidates,
        &import_identifiers,
        parsed.deferred_for_expressions(),
    )
    .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
    *plan_file.plan_mut() = rehydrated_plan;

    rehydrate_saved_resource_like(
        plan_file.sorted_resources_mut(),
        &resource_candidates,
        apply_target,
        carina_core::value::rehydrate_resource_secrets,
    )?;
    rehydrate_saved_resource_like(
        plan_file.unresolved_resources_mut(),
        &resource_candidates,
        apply_target,
        carina_core::value::rehydrate_resource_secrets,
    )?;

    let mut composition_source_identities = CompositionSourceIdentityResolver::new(
        carina_core::config_loader::get_base_dir(apply_target.source_dir()),
    )
    .map_err(AppError::from)?;
    plan_file
        .rehydrate_composition_secrets(
            &parsed.compositions,
            &mut composition_source_identities,
        )
        .map_err(|error| match error {
            ReloadedCompositionRehydrationError::MissingCandidate(origin_id) => {
                let call_chain = origin_id
                    .calls()
                    .map(|call| {
                        let source_file = call.source_file().to_string_lossy();
                        format!(
                            "{}:{}:{}",
                            source_file.escape_debug(),
                            call.start_line(),
                            call.start_column(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" -> ");
                saved_plan_secret_shape_error(
                    format!(
                        "composition call id chain {call_chain} disagreed with the saved plan",
                    ),
                    apply_target,
                )
            }
            ReloadedCompositionRehydrationError::Secret(error) => {
                saved_plan_secret_rehydration_error(error, apply_target)
            }
            ReloadedCompositionRehydrationError::SourceIdentity(error) => AppError::from(error),
        })?;

    rehydrate_saved_resource_like(
        plan_file.data_sources_mut(),
        &data_source_candidates,
        apply_target,
        carina_core::value::rehydrate_data_source_secrets,
    )?;
    rehydrate_saved_resource_like(
        plan_file.data_source_origins_mut(),
        &data_source_candidates,
        apply_target,
        carina_core::value::rehydrate_data_source_secrets,
    )?;

    let fresh_provider_configs =
        carina_core::parser::SavedProviderConfigs::from(provider_inputs.configs());
    plan_file
        .rehydrate_provider_config_secrets(&fresh_provider_configs)
        .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
    for (binding, attributes) in plan_file.upstream_snapshot_mut() {
        if let Some((key, _)) = attributes
            .iter()
            .find(|(_, value)| carina_core::value::contains_redacted_secret(value))
        {
            return Err(saved_plan_secret_shape_error(
                format!(
                    "upstream_state {binding}.{key}: secret placeholders cannot occur in valid upstream exports"
                ),
                apply_target,
            ));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_apply_from_plan(
    runtime: &crate::runtime::CommandRuntime,
    plan_path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    run_apply_from_plan_with_observer_factory(
        runtime,
        plan_path,
        auto_approve,
        lock,
        parallelism,
        accept_legacy_name_overrides,
        provider_context,
        cancel.clone(),
        &cli_observer_factory,
    )
    .await
}

#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
async fn run_apply_from_plan_with_observer_factory(
    runtime: &crate::runtime::CommandRuntime,
    plan_path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_, SavedModuleSource>,
) -> Result<(), AppError> {
    let color_mode = runtime.color_mode();
    let bytes = fs::read(plan_path).map_err(|error| AppError::PlanFileRead {
        path: plan_path.to_path_buf(),
        error,
    })?;
    let header: PlanFileHeader =
        serde_json::from_slice(&bytes).map_err(|error| match error.classify() {
            serde_json::error::Category::Data => AppError::InvalidPlanFile {
                path: plan_path.to_path_buf(),
                error,
            },
            serde_json::error::Category::Syntax
            | serde_json::error::Category::Eof
            | serde_json::error::Category::Io => AppError::UnreadableJsonPlanFile {
                path: plan_path.to_path_buf(),
                error,
            },
        })?;
    if bytes
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        != Some(b'{')
    {
        return Err(AppError::InvalidPlanFile {
            path: plan_path.to_path_buf(),
            error: <serde_json::Error as serde::de::Error>::custom(
                "expected a top-level JSON object",
            ),
        });
    }
    let replan_command = header
        .source_path
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .map(|source_path| saved_plan_replan_command(plan_path, Path::new(source_path)));

    // Validate version compatibility before decoding the payload. Format 37
    // requires a relative diagnostic file (or null) and line on each deferred-for
    // template. Format 36 adds typed deferred-for templates and stage-specific
    // placeholder types.
    // Format 35 stores tri-state prevent_destroy directives, so format 34
    // plans must be regenerated. Format 34 adds typed deferred integer
    // arithmetic expressions. Format 33 adds the typed keyed-list release
    // effect. Format 32 records provider
    // instance reference scopes and create-outcome-unknown markers for
    // protocol v13. Format 31 persists authored ignore paths; format 30 was
    // never emitted. Format 29 persists anonymous-loop provenance, typed
    // deferred-loop targets, and Move origins. Format 27 requires normalized
    // composition call origins. Plan-file version 26
    // persists the exact provider artifacts used to compute the plan. Plan-file
    // version 25 persists every data-source outcome so apply never re-derives
    // it from a partial graph.
    // Plan-file version 24 removes the
    // redundant wait target schema kind now that waits are managed-only.
    // Version 23 persists the provider-schema fingerprints checked before
    // saved-plan execution.
    // Version 22 persists deferred coalesce operands and complete deferred-for
    // iterable expressions. Version 21 represents an
    // in-flight managed current instance as an address-level marker and never
    // serializes its provider token; apply reloads that continuation from
    // locked state. Version 20 explicitly tags current-state ownership and
    // managed lifecycle classification but cannot represent the marker and
    // must be regenerated. Version 19 inferred ownership from the saved
    // data-source list and must likewise be regenerated. Version 19 guarantees
    // that mutation effects carry validated
    // provider identifiers, update effects persist their identifier explicitly,
    // and `removed` can record malformed deposed generations it abandons by key
    // without serializing their known-invalid identifier. Version 18 included
    // that redundant identifier and must be regenerated for the current shape.
    // Version 17 lacks the update identifier field and must likewise be
    // regenerated.
    // Version 17 guaranteed that state-originated secret placeholders came
    // from structural secret tags; v16 may contain marker-shaped user strings
    // that were misclassified and must be regenerated. Version 16 guarantees
    // that unquoted wait RHS
    // identifiers use the typed enum representation; v15
    // encoded them as Strings indistinguishable from intentionally quoted
    // comparisons and must be regenerated. Version 15 guarantees that secret
    // placeholders carry self-contained salts and that resource placeholders
    // can be reused byte-for-byte by state writeback. Version 14 guarantees
    // that embedded current states were created after legacy `_`-prefixed host
    // attributes moved behind the state migration boundary and orphan state
    // became structurally paired with its host metadata during planning.
    //
    // Plan-file version 12 stores secrets as typed placeholders and makes
    // saved-plan apply re-evaluate and verify them. A v11 plan's secret hashes
    // are indistinguishable from user strings and must be regenerated.
    //
    // Plan-file version 10 persists
    // `Effect::Delete.generation`; older binaries would silently
    // degrade deposed deletes to current deletes through serde's
    // additive default, so v9 plans must not cross this boundary.
    //
    // Plan-file version 8 removes the
    // legacy Replace effect shape; older saved plans must be regenerated.
    //
    // Plan-file version 6
    // (carina#3486) persists the pre-resolution resource snapshot used
    // by apply-time dependency analysis and reference re-resolution.
    //
    // Plan-file version 5
    // (carina#3329) changed `Effect::Import.identifier`'s on-disk shape
    // from a plain string to a tagged `Value`, so a deferred upstream-
    // state reference inside the import id can survive plan→apply as
    // a `Value::Deferred(Interpolation)` rather than being silently
    // substituted to empty. Pre-#3329 plans (version 4) serialise
    // `identifier` as a raw string and would otherwise hit an opaque
    // serde error at deserialisation; the explicit version check
    // produces a clean "re-run plan" message instead.
    //
    // Plan-file version 4 (carina#3248) persists `compositions` so the
    // saved-plan apply path can rebuild the same `ResolvedBindings`
    // view as the live-apply path (carina#3246). Older plans cannot be
    // applied by the post-#3248 binding-construction path and are
    // rejected outright per the repo's no-backward-compat policy.
    if header.version != PlanFile::CURRENT_VERSION {
        return Err(AppError::UnsupportedPlanVersion {
            path: plan_path.to_path_buf(),
            found: header.version,
            expected: PlanFile::CURRENT_VERSION,
            replan_command,
        });
    }

    let plan_file: PlanFile = (|| {
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
        let plan_file = serde_path_to_error::deserialize(&mut deserializer)
            .map_err(PlanFileJsonError::from_path_error)?;
        deserializer.end().map_err(PlanFileJsonError::at_root)?;
        Ok::<PlanFile, PlanFileJsonError>(plan_file)
    })()
    .map_err(|error| AppError::corrupt_plan_file(plan_path.to_path_buf(), replan_command, error))?;
    let source_path = std::path::PathBuf::from(&plan_file.source_path);
    let apply_target = ApplyTarget::SavedPlan {
        plan_file: plan_path,
        source_dir: &source_path,
    };
    plan_file
        .validate_resource_identities()
        .map_err(AppError::Config)?;
    plan_file
        .validate_replace_display()
        .map_err(AppError::Config)?;

    let current_version = env!("CARGO_PKG_VERSION");
    if plan_file.carina_version != current_version {
        println!(
            "{}",
            format!(
                "Warning: plan was created with carina version {} but current version is {}",
                plan_file.carina_version, current_version
            )
            .yellow(color_mode)
        );
    }

    println!(
        "{}",
        format!(
            "Using saved plan from {} (created {})",
            plan_file.source_path, plan_file.timestamp
        )
        .cyan(color_mode)
    );

    let base_dir = get_base_dir(&source_path);
    let inspected_plan = InspectedPlanFile::inspect(plan_file);
    // Load exactly once from the saved plan's provider configs: this is the
    // provider instance that will execute. Complete its diagnostics before
    // current-config secret validation, backend drift, saved-plan backend
    // mismatch, or the state-lock gate.
    let providers = ProviderLoad::load(
        runtime,
        inspected_plan.provider_configs().as_provider_configs(),
        base_dir,
    )?
    .into_complete()?;
    // Backend drift remains a mutation-safety gate even when the saved plan
    // has no secrets. Loading the syntax tree is deliberately separate from
    // validation and provider-config resolution: placeholder-free plans may
    // outlive unrelated validation errors in the source configuration (R5),
    // while a valid backend edit must still require migration or replanning.
    let loaded_current_config = load_configuration_with_config(
        &source_path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    );
    let (inspected_plan, parsed_backend) = match inspected_plan {
        InspectedPlanFile::Verified(plan) => {
            let parsed_backend = loaded_current_config
                .ok()
                .map(|config| {
                    config
                        .into_root_expanded()
                        .backend
                        .map(carina_core::parser::BackendConfig::into_expanded)
                        .transpose()
                })
                .transpose()?;
            (
                ProviderBoundPlanFile::verified(plan, providers),
                parsed_backend,
            )
        }
        InspectedPlanFile::NeedsSecretVerification(plan) => {
            let current_config = loaded_current_config.map_err(|source| {
                saved_plan_secret_evaluation_error(AppError::Config(source), apply_target)
            })?;
            let parsed = current_config.parsed;
            let unresolved = current_config.unresolved_parsed;
            let duplicate_declarations = current_config.duplicate_declarations;
            let providers = providers
                .check_saved_plan_config_pair(
                    parsed,
                    unresolved,
                    base_dir,
                    false,
                    &duplicate_declarations,
                )
                .map_err(|source| saved_plan_secret_evaluation_error(source, apply_target))?;
            let parsed_backend = Some(providers.parsed().backend.clone());
            (
                ProviderBoundPlanFile::needs_secret_verification(plan, providers),
                parsed_backend,
            )
        }
    };
    let current_backend = parsed_backend.as_ref().map(Option::as_ref);
    let backend_config = match current_backend {
        Some(current_backend) => current_backend,
        None => inspected_plan.backend_config(),
    };
    let verified_backend = verify_for_mutation(&source_path, backend_config, DriftCommand::Apply)?;
    // Gate order is deliberate: provider load diagnostics were completed
    // above; configuration-vs-lock drift is checked next, then the saved-plan
    // backend is compared with the current configuration, and only after both
    // backend gates pass is the state lock acquired.
    if let Some(current_backend) = current_backend {
        ensure_saved_plan_backend_matches_current(
            inspected_plan.backend_config(),
            current_backend,
            apply_target,
        )?;
    }

    // Set up backend
    let backend: Arc<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?
        .into();

    // Acquire lock (unless --lock=false)
    let state_lock = if lock {
        println!("{}", "Acquiring state lock...".cyan(color_mode));
        let li = backend
            .acquire_lock("apply")
            .await
            .map_err(|error| map_lock_error(error, &source_path))?;
        println!("  {} Lock acquired", "✓".green(color_mode));
        StateLock::held(li)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow(color_mode)
                .bold(color_mode)
        );
        StateLock::disabled_by_cli_flag()
    };

    let outcome = run_apply_from_plan_locked_owned(
        color_mode,
        inspected_plan,
        provider_context,
        auto_approve,
        Arc::clone(&backend),
        state_lock,
        apply_target,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await;
    let timing = finish_lock_command(color_mode, backend.as_ref(), outcome, &cancel)
        .await
        .map_err(|error| error.with_project_dir(&source_path))?;
    if let Some(elapsed) = timing {
        println!("{}", format_total_apply_line(elapsed));
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_apply_from_plan_locked_owned(
    color_mode: crate::output_style::ColorMode,
    inspected_plan: ProviderBoundPlanFile,
    provider_context: &ProviderContext,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    apply_target: ApplyTarget<'_>,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_, SavedModuleSource>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> LockCommandOutcome<Result<Option<Duration>, AppError>> {
    macro_rules! try_with_lock {
        ($expression:expr) => {
            match $expression {
                Ok(value) => value,
                Err(error) => {
                    return LockCommandOutcome::Kept {
                        result: Err(error),
                        lock,
                    };
                }
            }
        };
    }

    let project_dir = apply_target.source_dir();
    let legacy_data_source_origins = inspected_plan.data_source_origins().to_vec();
    // Read current state and validate lineage. carina#3315: a
    // pending in-memory schema migration must be persisted under
    // the apply lock so the carina#3283 warning text matches
    // reality — but we run that *after* the saved-plan
    // lineage/serial check below, so the operator sees the original
    // on-disk serial (the one the saved plan was diffed against)
    // rather than a +1 bump caused by our own migration persist.
    let (mut state_file, pending_migration) =
        match try_with_lock!(backend.read_state().await.map_err(AppError::Backend)) {
            None => (None, None),
            Some(carina_state::LoadedState::Pristine(state)) => (Some(state), None),
            Some(carina_state::LoadedState::Migrated { state, info }) => (Some(state), Some(info)),
        };
    if let Some(state) = state_file.as_ref() {
        try_with_lock!(check_legacy_name_overrides(
            state,
            accept_legacy_name_overrides,
            apply_target,
        ));
    }

    if let Some(ref state) = state_file {
        // Validate state lineage
        if let Some(plan_lineage) = inspected_plan.state_lineage()
            && state.lineage != plan_lineage
        {
            return LockCommandOutcome::Kept {
                result: Err(AppError::Config(format!(
                    "State lineage mismatch: plan was created for lineage '{}' but current state has '{}'",
                    plan_lineage, state.lineage
                ))),
                lock,
            };
        }

        // Warn on serial mismatch (state may have drifted)
        if let Some(plan_serial) = inspected_plan.state_serial()
            && state.serial != plan_serial
        {
            println!(
                "{}",
                format!(
                    "Warning: state serial has changed since plan was created ({} → {}). \
                     The infrastructure may have drifted.",
                    plan_serial, state.serial
                )
                .yellow(color_mode)
            );
        }
    }

    let inspected_plan = try_with_lock!(
        inspected_plan.reload_in_flight_from_locked_state(state_file.as_ref(), project_dir)
    );

    // A typed `read` address plus an identifier-less row shape distinguishes
    // historical pre-#3181 data-source rows without erasing an identifier-
    // bearing managed orphan at the same address. Normalize those artifacts
    // only after checking the saved plan against the original
    // on-disk serial, then persist that cleanup together with any schema
    // migration before drift detection can return early.
    let removed_legacy_data_source_rows = state_file
        .as_mut()
        .is_some_and(|state| state.remove_legacy_data_source_rows(&legacy_data_source_origins) > 0);
    if (pending_migration.is_some() || removed_legacy_data_source_rows)
        && let Some(state) = state_file.as_mut()
    {
        match lock.as_ref() {
            StateLockRef::Held(held) => {
                try_with_lock!(save_state_locked(backend.as_ref(), held, state).await);
            }
            StateLockRef::Disabled(disabled) => {
                try_with_lock!(save_state_unlocked(backend.as_ref(), disabled, state).await);
            }
        }
    }

    let heartbeat_outcome = with_lock_heartbeat(
        Arc::clone(&backend),
        lock,
        cancel,
        async |cancel, mid_run_writer| {
            // Artifact and schema verification consumes the preloaded provider
            // state. The secret-bearing variant returns one owner containing
            // both that exact load and its retained current-config pair, so
            // validation is not repeated under the lock and execution cannot
            // substitute either half.
            let (validated_plan, initialized_provider) = match inspected_plan {
                ReloadedProviderBoundPlanFile::Verified { plan, providers } => {
                    (plan.verify_provider_load(providers, apply_target)?, None)
                }
                ReloadedProviderBoundPlanFile::NeedsSecretVerification { plan, providers } => {
                    let (pending, validated_config) =
                        plan.verify_provider_load(providers, apply_target)?;
                    let prepared = prepare_saved_plan_secret_candidates(
                        validated_config,
                        &pending,
                        &state_file,
                        project_dir,
                        provider_context,
                        apply_target,
                    )
                    .await
                    .map_err(|source| saved_plan_secret_preparation_error(source, apply_target))?;
                    let PreparedSavedPlanSecretCandidates {
                        candidates,
                        provider,
                    } = prepared;
                    (
                        pending.verify_desired(candidates, apply_target)?,
                        Some(provider),
                    )
                }
            };
            let wiring = validated_plan.context();

            let resolved_provider_configs =
                ResolvedProviderConfigs::try_from(validated_plan.provider_configs().clone())
                    .map_err(|source| AppError::SavedProviderConfig(Box::new(source)))?;
            let raw_provider_configs = resolved_provider_configs.as_provider_configs();

            // Secret-bearing plans initialize once while reproducing the
            // provider-aware expansion used to verify desired values. Plans
            // without desired placeholders initialize here. Either way, the
            // same initialized instances continue through drift and execution.
            let provider = match initialized_provider {
                Some(provider) => provider,
                None => {
                    create_providers_from_configs_with_ctx(wiring, &resolved_provider_configs)
                        .await?
                }
            };
            let validated_plan = validated_plan.verify_provider_scopes(provider, apply_target)?;
            let wiring = validated_plan.context();

            // Drift detection: re-read actual infrastructure state and compare against planned states
            println!(
                "{}",
                "Checking for infrastructure drift...".cyan(color_mode)
            );
            let historical_addresses = validated_plan.historical_resource_addresses();
            let drift_check = detect_drift(
                color_mode,
                validated_plan.sorted_resources(),
                validated_plan.current_states(),
                &historical_addresses,
                validated_plan.provider(),
                validated_plan.provider(),
                state_file.as_ref(),
                wiring.schemas(),
                apply_target,
            )
            .await?;

            if let Some(drift_messages) = drift_check.drift_messages() {
                println!();
                println!(
                    "{}",
                    "Error: Infrastructure drift detected!"
                        .red(color_mode)
                        .bold(color_mode)
                );
                println!(
                    "{}",
                    "The following resources have changed since the plan was created:"
                        .red(color_mode)
                );
                println!();
                for msg in drift_messages {
                    println!("{}", msg);
                }
                println!();
                println!("{}", drift_replan_hint(apply_target).yellow(color_mode));
                return Err(AppError::Config(
                    "Apply aborted due to infrastructure drift.".to_string(),
                ));
            }

            println!("  {} No drift detected.", "✓".green(color_mode));

            let (mut actual_states, mut verified_planned_states) =
                drift_check.into_verified_states();

            let scope_checked_plan = validated_plan
                .verify_historical(
                    &mut actual_states,
                    &mut verified_planned_states,
                    &historical_addresses,
                    apply_target,
                )
                .await?;
            let (rehydrated_plan_file, provider) = scope_checked_plan.into_parts();
            let wiring = rehydrated_plan_file.context();

            let plan = rehydrated_plan_file.plan();
            let sorted_resources = rehydrated_plan_file.sorted_resources();
            let resolved_sorted_resources = ResolvedResource::collect_resolved_with_schemas(
                sorted_resources.iter().cloned(),
                wiring.schemas(),
            )?;
            let plan_compositions = rehydrated_plan_file.compositions();
            let plan_data_sources = rehydrated_plan_file.data_sources();
            let plan_data_source_origins: &[SavedDataSource] =
                rehydrated_plan_file.data_source_origins();

            let mut managed_current = HashMap::new();
            for entry in rehydrated_plan_file.current_states() {
                let crate::commands::plan::ReloadedCurrentStateEntry::Managed { id, current } =
                    entry;
                managed_current.insert(id.clone(), current.as_ref().clone());
            }
            let mut data_source_values = data_source_values_from_saved_plan(
                plan_data_sources,
                rehydrated_plan_file.data_source_values(),
            )?;
            render_plan_errors_and_abort(color_mode, plan, project_dir)?;
            let executable_data_source_values = data_source_values
                .clone()
                .into_executable()
                .map_err(|error| AppError::Config(error.to_string()))?;
            let directive_changes =
                persisted_directive_changes(state_file.as_ref(), &resolved_sorted_resources, plan);
            let recorded_directive_changes = rehydrated_plan_file
                .reloaded_plan_file()
                .persisted_directive_changes();
            if directive_changes != recorded_directive_changes {
                return Err(AppError::SavedPlanDirectiveChangesMismatch(Box::new(
                    SavedPlanDirectiveChangesMismatchError {
                        recorded: recorded_directive_changes.to_vec(),
                        recomputed: directive_changes,
                        replan_command: apply_target.replan_command().to_string(),
                    },
                )));
            }

            if can_use_export_only_fast_path(plan, &executable_data_source_values) {
                // Saved plans serialize every `Effect::Read` produced by the
                // differ; gating on `is_empty()` would skip this branch for
                // any data-source-bearing config, even when no managed
                // resource needs work, and fall through into the
                // resource-apply pipeline. Mirrors the source-driven apply
                // path's gate (carina#3270 → run_apply_locked).
                // carina#3275.
                let terminal_wait_aliases: Vec<WaitAliasSpec> = rehydrated_plan_file
                    .wait_bindings()
                    .iter()
                    .map(|wb| WaitAliasSpec {
                        binding: wb.binding.clone(),
                        target: wb.target.clone(),
                    })
                    .collect();
                let terminal_input_current = carina_core::resource::into_plan_input_current_map(
                    managed_current.clone(),
                    wiring.schemas(),
                    sorted_resources,
                );
                let terminal_bindings = ResolvedBindings::pre_apply_saved(SavedPreApplyInputs {
                    managed: sorted_resources,
                    compositions: plan_compositions,
                    managed_current: &terminal_input_current,
                    data_source_values: &data_source_values,
                    remote_bindings: rehydrated_plan_file.upstream_snapshot(),
                    wait_aliases: &terminal_wait_aliases,
                });
                finalize_saved_module_constraints(plan_compositions, &terminal_bindings)?;
                let directive_change_count = directive_changes.len();
                if directive_change_count == 0 {
                    println!("{}", "No changes needed.".green(color_mode));
                    return Ok(SavedPlanApplyAfterHeartbeat::Done(None));
                }

                let historical_display = collect_historical_display_attributes(
                    plan,
                    &managed_current,
                    state_file.as_ref(),
                );
                let summary = PlanChangeSummary::from_reloaded_plan(
                    rehydrated_plan_file.reloaded_plan_file(),
                );
                print_plan(
                    color_mode,
                    plan,
                    &summary,
                    DetailLevel::Full,
                    &historical_display,
                    Some(wiring.schemas()),
                    &HashMap::new(),
                    None,
                    None,
                    &provider,
                )?;
                let stdin = tokio::io::BufReader::new(tokio::io::stdin());
                if confirm_apply(wiring.runtime(), stdin, cancel.clone(), auto_approve).await?
                    == ApplyConfirmation::Cancelled
                {
                    return Ok(SavedPlanApplyAfterHeartbeat::Done(None));
                }
                println!(
                    "{}",
                    format!(
                        "Persisting {} resource directive change(s) to state.",
                        directive_change_count
                    )
                    .cyan(color_mode)
                );
                let mut state = state_file.take().unwrap_or_default();
                apply_persisted_directive_changes(
                    &mut state,
                    &resolved_sorted_resources,
                    &directive_changes,
                )?;
                return Ok(SavedPlanApplyAfterHeartbeat::PersistStateOnly(Box::new(
                    SavedPlanPersistStateOnly {
                        state,
                        directive_change_count,
                    },
                )));
            }

            // Build display-safe historical attributes from current and persisted state.
            let historical_display =
                collect_historical_display_attributes(plan, &managed_current, state_file.as_ref());

            let summary =
                PlanChangeSummary::from_reloaded_plan(rehydrated_plan_file.reloaded_plan_file());
            print_plan(
                color_mode,
                plan,
                &summary,
                DetailLevel::Full,
                &historical_display,
                Some(wiring.schemas()),
                &HashMap::new(),
                None,
                None,
                &provider,
            )?;
            let moved_hints = crate::moved_hint::detect_moved_block_hints(
                plan,
                &managed_current,
                wiring.schemas(),
                &provider,
            )?;
            moved_hints.eprint(color_mode);

            let stdin = tokio::io::BufReader::new(tokio::io::stdin());
            if confirm_apply(wiring.runtime(), stdin, cancel.clone(), auto_approve).await?
                == ApplyConfirmation::Cancelled
            {
                return Ok(SavedPlanApplyAfterHeartbeat::Done(None));
            }

            // Verify upstream-state bindings have not drifted since `carina
            // plan` ran (#2303). Re-load each upstream the plan declared and
            // compare against the persisted snapshot; if any binding's
            // attribute map disagrees, fail rather than silently mixing
            // plan-time and apply-time values during cascade re-resolution.
            let upstream_snapshot = rehydrated_plan_file.upstream_snapshot().clone();
            if !rehydrated_plan_file.upstream_sources().is_empty() {
                verify_upstream_snapshot(
                    wiring.runtime(),
                    rehydrated_plan_file.upstream_sources(),
                    &upstream_snapshot,
                    apply_target,
                )
                .await?;
            }
            // Rebuild the wait passthrough aliases from the persisted
            // `(binding, target)` pairs so `apply --plan`'s cascade
            // re-resolution resolves `<wait-binding>.<attr>` exactly as the
            // plan path did (carina#3085). `PlanWaitBinding` and
            // `WaitAliasSpec<AbsoluteBindingAddress>` both retain absolute
            // binding addresses; the parser `WaitBinding`
            // is not serializable so it is reconstructed here, not deserialized.
            let wait_aliases: Vec<WaitAliasSpec> = rehydrated_plan_file
                .wait_bindings()
                .iter()
                .map(|wb| WaitAliasSpec {
                    binding: wb.binding.clone(),
                    target: wb.target.clone(),
                })
                .collect();
            // carina#3248: saved plans (version 4) persist both
            // `compositions` and `data_sources`, so the saved-plan apply
            // path builds the same unified pre-apply bindings view as the
            // live-apply path. The view includes compositions so a managed
            // attribute referencing `<module_instance>.<attr>` chains through
            // to the managed sibling literal (carina#3246), and includes data
            // sources so a managed attribute referencing `<read_binding>.<attr>`
            // resolves through the data source's attribute map.
            let pre_apply_input_current = carina_core::resource::into_plan_input_current_map(
                managed_current.clone(),
                wiring.schemas(),
                sorted_resources,
            );
            let mut bindings = ResolvedBindings::pre_apply_saved(SavedPreApplyInputs {
                managed: sorted_resources,
                compositions: plan_compositions,
                managed_current: &pre_apply_input_current,
                data_source_values: &data_source_values,
                remote_bindings: &upstream_snapshot,
                wait_aliases: &wait_aliases,
            });
            bindings.apply_plan_stability(
                sorted_resources,
                &pre_apply_input_current,
                plan,
                wiring.schemas(),
            );

            let apply_phase_started = Instant::now();
            println!(
                "{}",
                "Applying changes...".cyan(color_mode).bold(color_mode)
            );
            println!();

            // Build unresolved resource map for re-resolution at apply time from
            // the saved pre-resolution snapshot, keyed by the paired sorted
            // resource IDs rather than the unresolved snapshot's own identity.
            let unresolved_resources = saved_plan_unresolved_by_resolved_id(
                sorted_resources,
                rehydrated_plan_file.unresolved_resources(),
                apply_target,
            )?;
            // Same object in both positions: `ProviderRouter` is both the
            // `Provider` and the `ProviderNormalizer`, so apply re-normalizes
            // with the plan-time normalizer (carina#3060).
            let observer = observer_factory(color_mode, plan, project_dir);
            let provider = Arc::new(provider);
            let state_save_inputs = Arc::new(ApplyStateSaveInputs::new(
                resolved_sorted_resources.clone(),
                (*plan).clone(),
                wiring.schemas().clone(),
                Some(rehydrated_plan_file.planned_secret_hashes().clone()),
                provider.as_ref(),
            )?);
            let saver =
                start_apply_state_saver(state_file, mid_run_writer, Arc::clone(&state_save_inputs));
            let publisher = saver.publisher();
            let checkpoint_sink = ExecutionCheckpointSink::new(move |snapshot| {
                publisher.send_replace(Some(snapshot));
            });
            // Remove, Move, and keyed-list release need no provider call.
            // Report them now; every checkpoint's state writeback materializes
            // them from the plan.
            let state_only_success_count = execute_state_only_effects(color_mode, plan);
            let outcome = execute_rehydrated_saved_plan(
                &rehydrated_plan_file,
                provider.as_ref(),
                provider.as_ref(),
                raw_provider_configs,
                wiring.factories(),
                wiring.schemas(),
                &mut bindings,
                &mut managed_current,
                executable_data_source_values,
                &unresolved_resources,
                plan_data_source_origins,
                plan_compositions,
                state_only_success_count,
                checkpoint_sink.clone(),
                cancel.clone(),
                parallelism,
                observer.as_ref(),
            )
            .await;
            let (mut result, cancelled) = split_execution_outcome(outcome);
            data_source_values
                .merge_read_states(&result.data_source_states)
                .map_err(|error| AppError::Config(error.to_string()))?;

            // Execute import effects: read imported resources from the provider
            execute_import_effects(
                color_mode,
                plan,
                provider.as_ref(),
                &mut result,
                &cancel,
                &checkpoint_sink,
                observer.as_ref(),
            )
            .await;
            let state_base = saver.stop(&cancel).await;

            Ok(SavedPlanApplyAfterHeartbeat::Executed(Box::new(
                SavedPlanApplyExecuted {
                    result,
                    cancelled,
                    resources_finished: Instant::now(),
                    apply_phase_started,
                    state_base,
                    state_save_inputs,
                    wait_aliases,
                    cancel,
                },
            )))
        },
    )
    .await;
    let (heartbeat_result, lock) = match heartbeat_outcome {
        HeartbeatOutcome::Kept { result, lock } => (result, lock),
        HeartbeatOutcome::Lost {
            result: _result,
            lost,
        } => {
            return LockCommandOutcome::Lost(AppError::StateLockLost {
                error: lost.into_error(),
            });
        }
    };

    let op_result = async {
        match heartbeat_result? {
            SavedPlanApplyAfterHeartbeat::Done(result) => Ok(result),
            SavedPlanApplyAfterHeartbeat::PersistStateOnly(context) => {
                let SavedPlanPersistStateOnly {
                    state,
                    directive_change_count,
                } = *context;
                persist_directives_only(
                    color_mode,
                    backend.as_ref(),
                    lock.as_ref(),
                    state,
                    directive_change_count,
                )
                .await?;
                Ok(None)
            }
            SavedPlanApplyAfterHeartbeat::Executed(context) => {
                let SavedPlanApplyExecuted {
                    result,
                    cancelled,
                    resources_finished,
                    apply_phase_started,
                    state_base,
                    state_save_inputs,
                    wait_aliases,
                    cancel,
                } = *context;
                let skipped_exports = finalize_after_execute(
                    |persistence| {
                        finalize_apply(
                            color_mode,
                            FinalizeApplyInput {
                                result: &result,
                                state_base,
                                state_save_inputs,
                                // Saved plan files do not persist export parameters today, so
                                // preserve `state.exports` rather than wiping it. Source-driven
                                // `carina apply` reconciles exports from the `.crn` directly
                                // (#2932).
                                export_resolution_inputs: None,
                                backend: backend.as_ref(),
                                lock: lock.as_ref(),
                                // No export resolution runs here, but pass the reconstructed
                                // aliases so the value is correct if a future plan file persists
                                // export parameters.
                                wait_aliases: &wait_aliases,
                                // No pre-resolve composition snapshot is needed without export
                                // resolution in the `apply --plan` path.
                                pre_resolve_compositions: &[],
                            },
                            persistence,
                        )
                    },
                    cancelled,
                    &cancel,
                )
                .await?;

                finish_apply(
                    color_mode,
                    &result,
                    skipped_exports,
                    resources_finished.duration_since(apply_phase_started),
                )
            }
        }
    }
    .await;

    LockCommandOutcome::Kept {
        result: op_result,
        lock,
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn run_apply_from_plan_locked(
    runtime: &crate::runtime::CommandRuntime,
    inspected_plan: InspectedPlanFile,
    provider_context: &ProviderContext,
    auto_approve: bool,
    backend: Arc<dyn StateBackend>,
    lock: StateLock,
    apply_target: ApplyTarget<'_>,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_, SavedModuleSource>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> Result<Option<Duration>, AppError> {
    let color_mode = runtime.color_mode();
    let inspected_plan = match ProviderLoad::load(
        runtime,
        inspected_plan.provider_configs().as_provider_configs(),
        apply_target.source_dir(),
    )
    .and_then(ProviderLoad::into_complete)
    .and_then(|providers| {
        ProviderBoundPlanFile::for_test(inspected_plan, providers, apply_target.source_dir())
    }) {
        Ok(inspected_plan) => inspected_plan,
        Err(error) => {
            return finish_lock_command(
                color_mode,
                backend.as_ref(),
                LockCommandOutcome::Kept {
                    result: Err(error),
                    lock,
                },
                &cancel,
            )
            .await;
        }
    };
    let outcome = run_apply_from_plan_locked_owned(
        color_mode,
        inspected_plan,
        provider_context,
        auto_approve,
        Arc::clone(&backend),
        lock,
        apply_target,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await;
    finish_lock_command(color_mode, backend.as_ref(), outcome, &cancel).await
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ApplyConfirmation {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyExitCode {
    Success = 0,
    Failure = 1,
    PartialSuccess = 2,
}

pub(crate) fn apply_exit_code_for_counts(
    failure_count: usize,
    partial_count: usize,
) -> ApplyExitCode {
    if failure_count > 0 {
        ApplyExitCode::Failure
    } else if partial_count > 0 {
        ApplyExitCode::PartialSuccess
    } else {
        ApplyExitCode::Success
    }
}

fn finish_apply<S>(
    color_mode: crate::output_style::ColorMode,
    result: &ExecutionResult<S>,
    skipped_exports: SkippedExports,
    elapsed: Duration,
) -> Result<Option<Duration>, AppError> {
    println!();
    let exit_code = apply_exit_code_for_counts(
        result.failure_count() + result.skip_count + result.interrupted_count,
        result.partial_count,
    );
    if exit_code == ApplyExitCode::Success && skipped_exports.is_empty() {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", result.success_count)
                .green(color_mode)
                .bold(color_mode)
        );
        return Ok(Some(elapsed));
    }

    let mut parts = vec![format!("{} succeeded", result.success_count)];
    if result.partial_count > 0 {
        parts.push(format!("{} partial", result.partial_count));
    }
    if result.failure_count() > 0 {
        parts.push(format!("{} failed", result.failure_count()));
    }
    if result.skip_count > 0 {
        parts.push(format!("{} skipped", result.skip_count));
    }
    if result.interrupted_count > 0 {
        parts.push(format!(
            "{} outcome{} unknown",
            result.interrupted_count,
            if result.interrupted_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if !skipped_exports.is_empty() {
        parts.push(skipped_exports.write_failure_description());
    }
    let export_hint = if skipped_exports.is_empty() {
        ""
    } else {
        " (see '! export ...' lines above)"
    };
    let message = format!("Apply failed. {}{}.", parts.join(", "), export_hint);

    if exit_code == ApplyExitCode::PartialSuccess {
        Err(AppError::PartialSuccess(message))
    } else {
        Err(crate::error::ExecutionFailureReport::new(message, result.failures()).into())
    }
}

/// Prompt the user to confirm an apply. Shared between the resource-change and
/// export-only paths so both use identical wording and behavior.
pub(crate) async fn confirm_apply<R>(
    runtime: &crate::runtime::CommandRuntime,
    reader: R,
    cancel: ShutdownToken,
    auto_approve: bool,
) -> Result<ApplyConfirmation, AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let color_mode = runtime.color_mode();
    if auto_approve {
        return Ok(ApplyConfirmation::Confirmed);
    }

    println!(
        "{}",
        "Do you want to perform these actions?"
            .yellow(color_mode)
            .bold(color_mode)
    );
    println!(
        "  {}",
        "Carina will perform the actions described above. Type 'yes' to confirm."
            .yellow(color_mode)
    );
    // The cursor is hidden command-wide (#3158); reveal it for the prompt
    // so the user does not type blind, and re-hide on scope exit.
    let input = {
        let _reveal = CursorReveal::new(runtime.cursor());
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let read_result = crate::signal::read_line_until_cancelled(reader, cancel).await;
        emit_newline_on_interrupt(&mut std::io::stdout(), &read_result);
        read_result?
    };

    if input.trim() != "yes" {
        println!();
        println!("{}", "Apply cancelled.".yellow(color_mode));
        Ok(ApplyConfirmation::Cancelled)
    } else {
        println!();
        Ok(ApplyConfirmation::Confirmed)
    }
}

#[cfg(test)]
mod tests;
