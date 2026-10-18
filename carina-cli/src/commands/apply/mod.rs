mod saved_plan;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::Path;
use std::time::{Duration, Instant};

use colored::Colorize;

use futures::stream::{self, StreamExt};

use carina_core::binding_index::{PreApplyInputs, ResolvedBindings, WaitAliasSpec};
use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{
    Orphans, PlanPriorStates, block_deletes_on_prior_consumer_updates, create_plan_with_cascades,
    into_all_prior_states, provider_states_for,
};
use carina_core::executor::{
    DeferredDataSourceReads, ExecutionInput, ExecutionObserver, ExecutionOutcome, ExecutionResult,
    ProviderCheckInputs, UnresolvedResource,
};
use carina_core::override_aware::{OverrideAwareResources, RebuildBindingsInputs};
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderNormalizer, ReadRequest};
#[cfg(test)]
use carina_core::resource::ConcreteValue;
use carina_core::resource::{DataSource, ResolvedResource, Resource, ResourceId, State, Value};
use carina_core::shutdown::ShutdownToken;
use carina_core::value::format_value;
use carina_state::{BackendLock, LockInfo, StateBackend, StateFile};

use carina_core::parser::{ProviderConfig, ProviderContext, ResolvedProviderConfigs};

use super::{DriftCommand, verify_for_mutation};
use crate::DetailLevel;
use crate::commands::plan::{
    CheckExportsPreApplyInput, PlanFile, PreparedPlanIdentityInputs, ProviderUpstreamScope,
    ResolvedProviderInputs, UpstreamMissingStatePolicy, collect_historical_display_attributes,
    load_and_resolve_provider_configs, prepare_plan_identity_and_provider_inputs,
};
use crate::commands::shared::effect_execution::{
    execute_import_effects, execute_state_only_effects,
};
use crate::commands::shared::finalize::{
    StatePersistence, finalize_after_execute, release_lock_after_execute,
};
use crate::commands::shared::observer::CliObserver;
use crate::commands::shared::plan_errors::render_plan_errors_and_abort;
use crate::commands::shared::progress::{
    RefreshProgress, emit_newline_on_interrupt, format_duration, refresh_multi_progress,
};
use crate::commands::shared::state_writeback::{
    ApplyStateSave, FinalizeApplyInput, PostApplyStates, ResolveExportValuesPostApplyInput,
    SkippedExports, build_state_after_apply, materialize_exports, resolve_export_values_post_apply,
};
use crate::commands::state::map_lock_error;
use crate::cursor::CursorReveal;
use crate::display::print_plan;
use crate::error::{AppError, SavedPlanSecretVerificationError};
use crate::wiring::{
    DataSourceRefreshResolution, LateAnonymousIdentityInputs, WiringContext,
    build_factories_from_providers, create_normalizers_from_configs_with_ctx,
    create_providers_from_configs_with_ctx, data_source_refresh_bindings, get_provider_with_ctx,
    prepare_data_sources_for_plan, prepare_default_provider_ready_config,
    prepare_desired_side_for_plan, read_data_source_with_retry, read_with_retry,
    reconcile_anonymous_identifiers_with_ctx, reconcile_late_anonymous_identities,
    reconcile_prefixed_names, resolve_data_source_refs_for_refresh,
};
use carina_core::hint::ProjectCommand;
use saved_plan::{
    DesiredPlanFile, HistoricalResourceAddresses, InspectedPlanFile, PlannedSecretHashes,
    RehydratedPlanFile,
};

/// Re-export ExecutionResult as the public API for apply results.
pub type ApplyResult = ExecutionResult;

type ObserverFactory<'a> = dyn Fn(&Plan) -> Box<dyn ExecutionObserver> + 'a;

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
    let location = error.location.clone();
    let replan = apply_target.replan_command().to_string();
    let error = match &error.source {
        carina_core::value::SecretRehydrationError::HashMismatch => {
            SavedPlanSecretVerificationError::HashMismatch {
                location,
                replan,
                source: error,
            }
        }
        carina_core::value::SecretRehydrationError::EvaluationUnavailable => {
            SavedPlanSecretVerificationError::SecretEvaluationUnavailable {
                location,
                replan,
                source: error,
            }
        }
        carina_core::value::SecretRehydrationError::OrphanVanished => {
            SavedPlanSecretVerificationError::OrphanVanished {
                resource: location,
                replan,
                source: error,
            }
        }
        _ => SavedPlanSecretVerificationError::ConfigurationChanged {
            location,
            replan,
            source: error,
        },
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
            source: Box::new(source),
        },
    ))
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

fn cli_observer_factory(plan: &Plan) -> Box<dyn ExecutionObserver> {
    Box::new(CliObserver::new(plan))
}

fn format_total_apply_line(elapsed: Duration) -> String {
    format!("Done in {}.", format_duration(elapsed))
}

async fn create_checked_bootstrap_resource(
    provider: &dyn Provider,
    resource: Resource,
    parsed: &carina_core::parser::InferredFile,
    provider_configs: &ResolvedProviderConfigs,
    normalizer: &dyn ProviderNormalizer,
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
) -> Result<(), AppError> {
    let id = resource.id.clone();
    let bindings = bootstrap_resolved_bindings(parsed);
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&parsed.compositions);
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
    provider
        .create(&id, request)
        .await
        .map_err(|source| AppError::StateBucketCreate { source })?;
    Ok(())
}

fn bootstrap_resolved_bindings(parsed: &carina_core::parser::InferredFile) -> ResolvedBindings {
    let current_states = HashMap::new();
    let remote_bindings = HashMap::new();
    let wait_aliases: Vec<WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(WaitAliasSpec::from)
        .collect();
    ResolvedBindings::pre_apply(PreApplyInputs {
        managed: &parsed.resources,
        compositions: &parsed.compositions,
        data_sources: &parsed.data_sources,
        current_states: &current_states,
        remote_bindings: &remote_bindings,
        wait_aliases: &wait_aliases,
    })
}

fn split_execution_outcome(outcome: ExecutionOutcome) -> (ExecutionResult, bool) {
    match outcome {
        ExecutionOutcome::Completed(result) => (result, false),
        ExecutionOutcome::Cancelled(result) => (result, true),
    }
}

fn finalize_module_constraints(
    compositions: &[carina_core::resource::Composition],
    bindings: &ResolvedBindings,
) -> Result<(), AppError> {
    carina_core::executor::ModuleConstraintGate::new(compositions)
        .finish(bindings, true)
        .map_err(AppError::from)
}

fn deferred_data_source_reads_from_data_sources(
    data_sources: &[DataSource],
    managed: &[Resource],
    current_states: &HashMap<ResourceId, State>,
) -> DeferredDataSourceReads {
    let mut deferred_data_source_reads = DeferredDataSourceReads::none();
    for (id, unresolved) in crate::wiring::classify_apply_time_data_source_read_inputs(
        data_sources,
        managed,
        current_states,
    ) {
        deferred_data_source_reads.insert(id, unresolved);
    }
    deferred_data_source_reads
}

fn saved_plan_unresolved_by_resolved_id(
    sorted_resources: &[Resource],
    unresolved_resources: &[Resource],
    apply_target: ApplyTarget<'_>,
) -> Result<HashMap<ResourceId, UnresolvedResource>, AppError> {
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

fn can_use_export_only_fast_path(
    plan: &Plan,
    deferred_data_source_reads: &DeferredDataSourceReads,
) -> bool {
    !plan.has_mutations() && deferred_data_source_reads.is_empty()
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

/// Execute all effects in a plan, resolving references dynamically.
///
/// This delegates to `carina_core::executor::execute_plan()` with a `CliObserver`
/// for colored progress output.
#[allow(clippy::too_many_arguments)]
pub async fn execute_effects(
    plan: &Plan,
    provider: &dyn Provider,
    normalizer: &dyn ProviderNormalizer,
    provider_configs: &[ProviderConfig],
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
    bindings: &mut ResolvedBindings,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    data_source_origins: &[DataSource],
    compositions: &[carina_core::resource::Composition],
    cancel: ShutdownToken,
    parallelism: NonZeroUsize,
) -> ExecutionOutcome {
    let observer = cli_observer_factory(plan);
    execute_effects_with_observer(
        plan,
        provider,
        normalizer,
        provider_configs,
        factories,
        schemas,
        bindings,
        current_states,
        unresolved_resources,
        data_source_origins,
        compositions,
        DeferredDataSourceReads::none(),
        cancel.clone(),
        parallelism,
        observer,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_effects_with_observer(
    plan: &Plan,
    provider: &dyn Provider,
    normalizer: &dyn ProviderNormalizer,
    provider_configs: &[ProviderConfig],
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
    bindings: &mut ResolvedBindings,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    data_source_origins: &[DataSource],
    compositions: &[carina_core::resource::Composition],
    deferred_data_source_reads: DeferredDataSourceReads,
    cancel: ShutdownToken,
    parallelism: NonZeroUsize,
    observer: Box<dyn ExecutionObserver>,
) -> ExecutionOutcome {
    let input = ExecutionInput {
        plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: unresolved_resources,
            data_source_origins,
        },
        compositions,
        bindings: bindings.clone(),
        current_states: std::mem::take(current_states),
        deferred_data_source_reads,
        normalizer,
        provider_configs,
        factories,
        schemas,
        parallelism,
    };

    let outcome = carina_core::executor::execute_plan(provider, input, &*observer, cancel).await;

    // Write back the updated current_states so callers see refreshes
    let result = match &outcome {
        ExecutionOutcome::Completed(result) | ExecutionOutcome::Cancelled(result) => result,
    };
    *current_states = result.current_states.clone();
    *bindings = result.bindings.clone();

    outcome
}

/// Saved-plan executor gateway. Requiring [`RehydratedPlanFile`] here makes it
/// impossible for the deserialized `PlanFile` path to dispatch effects before
/// all typed secret placeholders have been verified and removed.
#[allow(clippy::too_many_arguments)]
async fn execute_rehydrated_saved_plan(
    plan_file: &RehydratedPlanFile,
    provider: &dyn Provider,
    normalizer: &dyn ProviderNormalizer,
    provider_configs: &[ProviderConfig],
    factories: &[Box<dyn carina_core::provider::ProviderFactory>],
    schemas: &carina_core::schema::SchemaRegistry,
    bindings: &mut ResolvedBindings,
    current_states: &mut HashMap<ResourceId, State>,
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    data_source_origins: &[DataSource],
    compositions: &[carina_core::resource::Composition],
    deferred_data_source_reads: DeferredDataSourceReads,
    cancel: ShutdownToken,
    parallelism: NonZeroUsize,
    observer: Box<dyn ExecutionObserver>,
) -> ExecutionOutcome {
    execute_effects_with_observer(
        plan_file.plan(),
        provider,
        normalizer,
        provider_configs,
        factories,
        schemas,
        bindings,
        current_states,
        unresolved_resources,
        data_source_origins,
        compositions,
        deferred_data_source_reads,
        cancel,
        parallelism,
        observer,
    )
    .await
}

/// Re-load each upstream declared in the saved plan and verify its
/// attribute map matches the snapshot the plan was computed against
/// (#2303). Fails on the first drifted binding so the user gets an
/// actionable message naming what changed; pretending the apply will
/// succeed and silently mixing plan-time and apply-time values
/// produces incorrect cascade re-resolution.
async fn verify_upstream_snapshot(
    sources: &[crate::commands::plan::UpstreamSource],
    snapshot: &HashMap<String, HashMap<String, Value>>,
    apply_target: ApplyTarget<'_>,
) -> Result<(), AppError> {
    use carina_core::parser::UpstreamState;
    let project_dir = apply_target.source_dir();
    let upstream_states: Vec<UpstreamState> = sources
        .iter()
        .map(|s| UpstreamState {
            binding: s.binding.clone(),
            source: s.source.clone(),
        })
        .collect();
    let provider_context = ProviderContext::default();
    let mut cycle_guard = super::plan::seed_cycle_guard(project_dir);
    let current = super::plan::load_upstream_states(
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
    provider: &dyn Provider,
    current_states: &mut HashMap<ResourceId, State>,
    pending_refreshes: &HashMap<ResourceId, String>,
) -> HashSet<ResourceId> {
    if pending_refreshes.is_empty() {
        return HashSet::new();
    }

    println!();
    println!("{}", "Refreshing uncertain resource states...".cyan());

    let mut refreshes: Vec<_> = pending_refreshes.iter().collect();
    refreshes.sort_by_key(|(left_id, _)| left_id.to_string());
    let mut failed_refreshes = HashSet::new();

    for (id, identifier) in refreshes {
        match read_with_retry(provider, id, Some(identifier)).await {
            Ok(state) => {
                println!("  {} Refresh {}", "✓".green(), id);
                current_states.insert(id.clone(), state);
            }
            Err(error) => {
                println!("  {} Refresh {} - {}", "!".yellow(), id, error);
                failed_refreshes.insert(id.clone());
            }
        }
    }

    failed_refreshes
}

/// Save state after apply. Does NOT release the lock -- caller is responsible.
///
/// When `lock` is `None` (i.e. `--lock=false`), state is written without lock
/// validation via `save_state_unlocked`.
///
/// Returns the [`SkippedExports`] summary after state is saved. Callers must fold
/// unresolved exports into the command's exit status (carina#3710).
pub(crate) async fn finalize_apply(
    input: FinalizeApplyInput<'_>,
    persistence: StatePersistence,
    planned_secret_hashes: Option<&PlannedSecretHashes>,
) -> Result<SkippedExports, AppError> {
    println!();
    println!("{}", "Saving state...".cyan());

    let save = ApplyStateSave {
        state_file: input.state_file,
        sorted_resources: input.sorted_resources,
        runtime_synthesized_resources: &input.result.runtime_synthesized_resources,
        current_states: input.current_states,
        applied_states: &input.result.applied_states,
        plan: input.plan,
        successfully_deleted: &input.result.successfully_deleted,
        failed_refreshes: &input.result.failed_refreshes,
        schemas: input.schemas,
    };
    let mut state = build_state_after_apply(save, planned_secret_hashes)?;

    // `Some([])` is meaningful: the user removed the `exports {}` block,
    // so stale entries the previous apply persisted must be cleared
    // (#2932). `None` preserves `state.exports` because the caller has
    // no source-side view of which exports the user intends — see the
    // `FinalizeApplyInput::export_params` doc-comment.
    let export_resolution = if let Some(params) = input.export_params {
        let post_apply_states =
            PostApplyStates::from_current_and_state(input.current_states, &state);
        let export_resources: Vec<Resource> = input
            .sorted_resources
            .iter()
            .map(|resource| resource.as_resource().clone())
            .collect();
        resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
            export_params: params,
            sorted_resources: &export_resources,
            data_sources: input.data_sources,
            pre_resolve_compositions: input.pre_resolve_compositions,
            post_apply_states: &post_apply_states,
            schemas: input.schemas,
            wait_aliases: input.wait_aliases,
            provider_context: input.provider_context,
        })
        .and_then(|values| materialize_exports(&values))
        .map(|resolution| resolution.write_into(&mut state))
    } else {
        Ok(SkippedExports::default())
    };

    // A provider mutation has already happened. Export materialization is a
    // secondary concern, so an export error must not prevent durable resource
    // writeback. Save the resource state first, then return the original
    // export error to the caller.
    if let Some(lock) = input.lock {
        save_state_locked_after_execute(input.backend, lock, &mut state, persistence).await?;
    } else {
        save_state_unlocked_after_execute(input.backend, &mut state, persistence).await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    export_resolution
}

/// Renew the lock and write state with lock validation.
///
/// This ensures that the lock is still held before writing state, preventing
/// silent state corruption when a lock has expired and been acquired by another
/// process during a long-running operation.
pub async fn save_state_locked(
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
pub async fn save_state_unlocked(
    backend: &dyn StateBackend,
    state: &mut StateFile,
) -> Result<(), AppError> {
    state.increment_serial();
    backend.write_state(state).await.map_err(AppError::Backend)
}

pub(crate) async fn save_state_unlocked_after_execute(
    backend: &dyn StateBackend,
    state: &mut StateFile,
    persistence: StatePersistence,
) -> Result<(), AppError> {
    match persistence {
        StatePersistence::Normal => save_state_unlocked(backend, state).await,
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
pub async fn load_state_persist_if_migrated(
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
) -> Result<Option<StateFile>, AppError> {
    match backend.read_state().await.map_err(AppError::Backend)? {
        None => Ok(None),
        Some(carina_state::LoadedState::Pristine(state)) => Ok(Some(state)),
        Some(carina_state::LoadedState::Migrated { mut state, .. }) => {
            if let Some(lk) = lock {
                save_state_locked(backend, lk, &mut state).await?;
            } else {
                save_state_unlocked(backend, &mut state).await?;
            }
            Ok(Some(state))
        }
    }
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
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    state_file: Option<StateFile>,
    sorted_resources: &[Resource],
    data_sources: &[carina_core::resource::DataSource],
    pre_resolve_compositions: &[carina_core::resource::Composition],
    export_params: &carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::InferredExportParam,
    >,
    schemas: &carina_core::schema::SchemaRegistry,
    wait_aliases: &[carina_core::binding_index::WaitAliasSpec],
    current_states: &HashMap<ResourceId, carina_core::resource::State>,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let mut state = state_file.unwrap_or_default();
    let post_apply_states = PostApplyStates::from_current_and_state(current_states, &state);
    let values = resolve_export_values_post_apply(ResolveExportValuesPostApplyInput {
        export_params,
        sorted_resources,
        data_sources,
        pre_resolve_compositions,
        post_apply_states: &post_apply_states,
        schemas,
        wait_aliases,
        provider_context,
    })?;
    let resolution = materialize_exports(&values)?;
    let skipped_exports = resolution.write_into(&mut state);
    if let Some(lk) = lock {
        save_state_locked(backend, lk, &mut state).await?;
    } else {
        save_state_unlocked(backend, &mut state).await?;
    }
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);
    if skipped_exports.is_empty() {
        println!("  {} Exports updated", "✓".green());
        Ok(())
    } else {
        println!("  {} {}", "!".yellow(), skipped_exports.count_description());
        Err(AppError::Config(skipped_exports.failure_message()))
    }
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
pub(crate) async fn detect_drift(
    sorted_resources: &[Resource],
    planned_state_entries: &[crate::commands::plan::CurrentStateEntry],
    historical_addresses: &HistoricalResourceAddresses,
    provider: &dyn Provider,
    apply_target: ApplyTarget<'_>,
) -> Result<DriftCheckResult, AppError> {
    let mut drift_detected = false;
    let mut drift_messages: Vec<String> = Vec::new();
    let mut actual_states = HashMap::new();
    let mut verified_planned_states = HashMap::new();
    let planned_states: HashMap<_, _> = planned_state_entries
        .iter()
        .map(|entry| (&entry.id, &entry.state))
        .collect();

    for resource in sorted_resources {
        // carina#3181: `sorted_resources` is managed-only — drift
        // detection only operates over provider-backed managed
        // resources.
        let planned_state = planned_states.get(&resource.id).copied();
        let identifier = planned_state.and_then(|s| s.identifier.as_deref());
        let historical_id = planned_state
            .map(|_| historical_addresses.origin_for(&resource.id))
            .unwrap_or(&resource.id);

        let mut actual_state = provider
            .read(historical_id, identifier, ReadRequest)
            .await
            .map_err(AppError::Provider)?;
        // The provider read uses the pre-move address, but every downstream
        // saved-plan map is keyed by the moved-to runtime address.
        actual_state.id = resource.id.clone();
        actual_states.insert(resource.id.clone(), actual_state.clone());

        if let Some(planned) = planned_state {
            if planned.exists != actual_state.exists {
                drift_detected = true;
                if planned.exists {
                    drift_messages.push(format!(
                        "  {} {}: resource existed at plan time but no longer exists",
                        "~".yellow(),
                        resource.id
                    ));
                } else {
                    drift_messages.push(format!(
                        "  {} {}: resource did not exist at plan time but now exists",
                        "~".yellow(),
                        resource.id
                    ));
                }
            } else if planned.exists && actual_state.exists {
                let verification =
                    carina_core::value::verify_historical_state_secrets(planned, &actual_state)
                        .map_err(|error| {
                            saved_plan_secret_rehydration_error(error, apply_target)
                        })?;
                let secret_drift = verification.drifted_attributes();
                let verified_planned = verification.state();
                verified_planned_states.insert(resource.id.clone(), verified_planned.clone());
                // Compare attributes for existing resources
                let mut attr_diffs: Vec<String> = Vec::new();
                for (key, planned_val) in &verified_planned.attributes {
                    let historical_shape = planned_state
                        .and_then(|state| state.attributes.get(key))
                        .unwrap_or(planned_val);
                    let masked_planned = carina_core::value::mask_plaintext_using_secret_shape(
                        planned_val,
                        historical_shape,
                    );
                    match actual_state.attributes.get(key) {
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
                        Some(actual_val) if actual_val != planned_val => {
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
                for (key, actual_val) in &actual_state.attributes {
                    if !planned.attributes.contains_key(key) {
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
                        "~".yellow(),
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

fn drift_replan_hint(apply_target: ApplyTarget<'_>) -> String {
    format!(
        "Please re-run `{}` to create a new plan that reflects the current state.",
        apply_target.replan_command()
    )
}

pub async fn run_apply(
    path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    run_apply_with_observer_factory(
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
    path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
) -> Result<(), AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;
    let backend_file = loaded.backend_file;

    let base_dir = get_base_dir(path);
    crate::commands::validate_and_resolve_with_config(
        &mut parsed,
        base_dir,
        false,
        &inference_errors,
        &duplicate_declarations,
    )?;

    // Wiring needs declaration metadata and schemas before a bootstrap
    // provider may be created. Provider inputs are loaded lazily in that
    // branch; ordinary apply reaches backend verification without touching
    // upstream state and resolves once in the lock-held path.
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
    let ctx = WiringContext::new(factories, &parsed.providers);

    let verified_backend = verify_for_mutation(path, parsed.backend.as_ref(), DriftCommand::Apply)?;

    // Check for backend configuration - use local backend by default
    let backend: Box<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

    // Handle bootstrap if S3 backend is configured
    #[allow(unused_assignments)]
    let mut lock_info: Option<LockInfo> = None;

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
                    .yellow()
                    .bold()
            );

            // Check if there's a bucket resource defined with matching name
            let backend_resource_type = backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            if let Some(bucket_resource) =
                parsed.find_resource_by_attr(backend_resource_type, "bucket", &bucket_name)
            {
                println!("Found state bucket resource in configuration.");
                println!(
                    "Creating bucket '{}' before other resources...",
                    bucket_name.cyan()
                );

                // Create the bucket resource using the factory pattern
                let backend_provider_name = backend
                    .provider_name()
                    .ok_or("Backend does not specify a provider name")?;
                // This is the only pre-lock path that constructs a provider.
                // Resolve its attributes here; every other apply path waits
                // for the post-reconciliation lock-held resolution below.
                let bootstrap_provider_inputs =
                    load_apply_provider_inputs(&parsed, base_dir, provider_context).await?;
                let (bucket_provider, bucket_normalizer) = create_state_bucket_bootstrap_provider(
                    &ctx,
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
                    bucket_provider.as_ref(),
                    bucket_resource.clone(),
                    &parsed,
                    bootstrap_provider_inputs.configs(),
                    bucket_normalizer.as_ref(),
                    ctx.factories(),
                    ctx.schemas(),
                )
                .await?;
                println!("  {} Created state bucket: {}", "✓".green(), bucket_name);
            } else {
                // Auto-create the bucket if auto_create is enabled
                if auto_create {
                    println!("Auto-creating state bucket: {}", bucket_name.cyan());
                    backend.create_bucket().await.map_err(AppError::Backend)?;
                    println!("  {} Created state bucket", "✓".green());

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
                        "✓".green(),
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
                    // Keep the authored pre-injection `unresolved_parsed` for
                    // resource pairing; replace only the resolved snapshot so
                    // the lock-held path sees the injected bucket row.
                    parsed = reload_apply_configuration_after_bootstrap(
                        path,
                        base_dir,
                        provider_context,
                    )?;

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
                        &bucket_name,
                    );
                    backend
                        .write_state(&initial_state)
                        .await
                        .map_err(AppError::Backend)?;
                    println!(
                        "  {} Registered state bucket as protected resource",
                        "✓".green()
                    );
                } else {
                    return Err(AppError::Config(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    )));
                }
            }

            // Initialize state if not already done (when bucket existed or was created from resource)
            if backend
                .read_state()
                .await
                .map_err(AppError::Backend)?
                .is_none()
            {
                backend.init().await.map_err(AppError::Backend)?;
            }
        }
    }

    // Acquire lock (unless --lock=false)
    if lock {
        println!("{}", "Acquiring state lock...".cyan());
        lock_info = Some(
            backend
                .acquire_lock("apply")
                .await
                .map_err(|error| map_lock_error(error, path))?,
        );
        println!("  {} Lock acquired", "✓".green());
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
    }

    // All code after lock acquisition observes the shared cancellation token
    // so lock release is guaranteed after SIGINT/SIGTERM cancellation.
    let op_result = run_apply_locked(
        &ctx,
        &mut parsed,
        &unresolved_parsed,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
        path,
        provider_context,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await;

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = release_lock_after_execute(backend.as_ref(), li, &cancel).await;

        if release_result.is_ok()
            && (op_result.is_ok() || matches!(op_result, Err(AppError::Interrupted)))
        {
            println!("  {} Lock released", "✓".green());
        }

        let timing = op_result?;
        release_result?;
        if let Some(elapsed) = timing {
            println!("{}", format_total_apply_line(elapsed));
        }
    } else {
        if let Some(timing) = op_result? {
            println!("{}", format_total_apply_line(timing));
        }
    }

    Ok(())
}

fn reload_apply_configuration_after_bootstrap(
    path: &Path,
    base_dir: &Path,
    provider_context: &ProviderContext,
) -> Result<carina_core::parser::InferredFile, AppError> {
    let reloaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = reloaded.inference_errors;
    let duplicate_declarations = reloaded.duplicate_declarations;
    let mut parsed = reloaded.parsed;
    crate::commands::validate_and_resolve_with_config(
        &mut parsed,
        base_dir,
        true,
        &inference_errors,
        &duplicate_declarations,
    )?;
    Ok(parsed)
}

async fn load_apply_provider_inputs<E>(
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
    provider_context: &ProviderContext,
) -> Result<ResolvedProviderInputs, AppError> {
    load_and_resolve_provider_configs(
        parsed,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
        ProviderUpstreamScope::All,
    )
    .await
}

async fn create_state_bucket_bootstrap_provider(
    ctx: &WiringContext,
    provider_configs: &ResolvedProviderConfigs,
    provider_name: &str,
) -> Result<
    (
        Box<dyn Provider>,
        Box<dyn carina_core::provider::ProviderNormalizer>,
    ),
    AppError,
> {
    let factory = provider_mod::find_factory(ctx.factories(), provider_name)
        .ok_or_else(|| format!("No provider factory found for '{provider_name}'"))?;
    let config = prepare_default_provider_ready_config(factory, provider_name, provider_configs)?;
    let provider = factory.create_provider(None, &config).await?;
    let normalizer = factory.create_normalizer(None, &config).await?;
    Ok((provider, normalizer))
}

#[allow(clippy::too_many_arguments)]
async fn run_apply_locked(
    ctx: &WiringContext,
    parsed: &mut carina_core::parser::InferredFile,
    unresolved_parsed: &carina_core::parser::ParsedFile,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    project_dir: &Path,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> Result<Option<Duration>, AppError> {
    // Read current state from backend. carina#3315: if `check_and_migrate`
    // lifted an older on-disk schema in memory, persist the upgrade
    // under the current lock before any short-circuit path can return
    // — otherwise the carina#3283 warning text ("Disk state will be
    // rewritten during the next apply or state refresh of that project.")
    // becomes a lie.
    let mut state_file = load_state_persist_if_migrated(backend, lock).await?;
    if let Some(state) = state_file.as_ref() {
        check_legacy_name_overrides(
            state,
            accept_legacy_name_overrides,
            ApplyTarget::Project(project_dir),
        )?;
    }

    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    let crate::wiring::StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = crate::wiring::resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        ctx.schemas(),
    );
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &mut parsed.compositions,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.identity.to_string())
                    .collect()
            },
            &state_block_claims,
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(
            ctx,
            &mut parsed.resources,
            sf,
            &state_block_claims,
        )?;
    }
    let provider_inputs = load_apply_provider_inputs(parsed, project_dir, provider_context).await?;
    let provider_configs = provider_inputs.configs();
    let raw_provider_configs = provider_configs.as_provider_configs();
    let remote_bindings = provider_inputs.remote_bindings();
    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(ctx, provider_configs, project_dir).await?;

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
    // refreshed in a later phase against the populated `current_states`.
    let mut sorted_resources = sort_resources_by_dependencies(&parsed.resources)?;
    let data_sources: Vec<carina_core::resource::DataSource> = parsed.data_sources.clone();

    // Build state-file-derived maps up front so anonymous → let-bound
    // rename transfer (#1685) can run between refresh phases 1 and 2.

    // awscc#251: lift pre-Enum-migration state (`ConcreteValue::
    // String` at a now-`Enum` position) before the differ /
    // `hydrate_read_state` consume it, so apply does not diff lifted
    // desired against un-lifted saved state. Same seam as the plan path.
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default()
        .lift(ctx.schemas());
    let mut prev_explicit = state_file
        .as_ref()
        .map(|sf| sf.build_explicit())
        .unwrap_or_default();

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    // Skip composition resources (module attribute containers) — they have no infrastructure.
    RefreshProgress::start_header();
    let multi = refresh_multi_progress();
    let provider_ref = &provider;
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    // Pre-build dependency_bindings from state file so we can restore them
    // after refresh. Provider.read() doesn't know about this metadata (#1565).
    let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
        .as_ref()
        .map(|sf| {
            sorted_resources
                .iter()
                .filter_map(|r| {
                    let identity = r.id.identity_str()?;
                    let rs = sf.find_resource(&r.id.provider, &r.id.resource_type, identity)?;
                    if rs.dependency_bindings.is_empty() {
                        None
                    } else {
                        Some((r.id.clone(), rs.dependency_bindings.clone()))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Phase 1: refresh managed resources in parallel. carina#3181:
    // `sorted_resources` is managed-only — data sources are refreshed in
    // phase 2 below.
    let phase1_results: Vec<Result<(ResourceId, State), AppError>> =
        stream::iter(sorted_resources.iter())
            .map(|resource| {
                let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                let identifier = state_file
                    .as_ref()
                    .and_then(|sf| sf.get_identifier_for_resource(resource));
                let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                async move {
                    let mut state =
                        read_with_retry(provider_ref, &resource.id, identifier.as_deref())
                            .await
                            .map_err(AppError::Provider)?;
                    if let Some(deps) = dep_bindings {
                        state.dependency_bindings = deps;
                    }
                    progress.finish();
                    Ok((resource.id.clone(), state))
                }
            })
            .buffer_unordered(5)
            .collect()
            .await;
    for result in phase1_results {
        let (id, state) = result?;
        current_states.insert(id, state);
    }

    // Refresh orphaned resources (#844, #1685). Must run before the
    // rename transfer below so old-name entries are present for
    // `apply_anonymous_to_named_renames` to transfer.
    let mut orphans = Orphans::new();
    // Ids the orphan pass already live-read this run. A for-loop child
    // applied previously is in state but not yet a desired resource at
    // orphan time (expansion is post-refresh); the orphan pass live-reads
    // it under the same state name the child resolves to.
    // `expand_same_config_deferred_for` excludes these from the
    // post-expansion child refresh so the same address is not read twice
    // (carina#3145). Mirrors the plan path in `wiring/mod.rs`.
    let mut orphan_refreshed_ids: HashSet<ResourceId> = HashSet::new();
    if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        orphans = sf.build_orphans(&desired_ids);
        let orphan_results: Vec<Result<(ResourceId, carina_core::differ::Orphan), AppError>> =
            stream::iter(std::mem::take(&mut orphans))
                .map(|(id, orphan)| async move {
                    let refreshed =
                        read_with_retry(provider_ref, &id, orphan.state().identifier.as_deref())
                            .await
                            .map_err(AppError::Provider)?;
                    Ok((id, orphan.with_refreshed_state(refreshed)))
                })
                .buffer_unordered(5)
                .collect()
                .await;
        for result in orphan_results {
            let (id, orphan) = result?;
            if orphan.state().exists {
                orphan_refreshed_ids.insert(id.clone());
            }
            orphans.insert(orphan);
        }
    }

    // Hydrate, transfer state for moved blocks and anonymous → let-bound
    // renames (#1685), then run phase 2 against the consolidated state.
    crate::wiring::hydrate_prior_states(
        &mut current_states,
        &mut orphans,
        &provider,
        &saved_attrs,
        state_file.as_ref(),
    )
    .await?;
    let moved_pairs = {
        let mut pairs = crate::wiring::materialize_moved_states(
            &mut current_states,
            &mut orphans,
            &mut prev_explicit,
            &mut saved_attrs,
            &parsed.state_blocks,
            &state_file,
        );
        pairs.extend(crate::wiring::apply_anonymous_to_named_renames(
            ctx,
            &sorted_resources,
            raw_provider_configs,
            &mut current_states,
            &mut orphans,
            &mut prev_explicit,
            &mut saved_attrs,
            &state_file,
            &state_block_claims,
        )?);
        pairs
    };
    // Wait bindings become passthrough aliases to their targets
    // (carina#3085). Built once here so every resolution phase below
    // (data-source refresh, initial bindings, ref resolution, exports)
    // applies the same passthrough.
    let wait_aliases: Vec<WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(WaitAliasSpec::from)
        .collect();

    // Phase 2: resolve data source inputs against the consolidated state
    // and refresh them via `read_data_source` (#1683, #1685).
    let data_source_bindings = data_source_refresh_bindings(
        &sorted_resources,
        &parsed.compositions,
        &data_sources,
        &current_states,
        remote_bindings,
        ctx.schemas(),
        &wait_aliases,
    );
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&parsed.compositions);
    let data_source_refreshes = resolve_data_source_refs_for_refresh(
        &sorted_resources,
        &parsed.compositions,
        &data_sources,
        &current_states,
        remote_bindings,
        ctx.schemas(),
        &wait_aliases,
    )?;
    let mut deferred_data_source_reads = DeferredDataSourceReads::none();
    let mut resolved_data_sources = Vec::new();
    for resolution in data_source_refreshes {
        match resolution {
            DataSourceRefreshResolution::Resolved(resource) => {
                resolved_data_sources.push(resource);
            }
            DataSourceRefreshResolution::DeferredToApply {
                resource,
                unresolved,
            } => {
                deferred_data_source_reads.insert(resource.id.clone(), unresolved);
            }
        }
    }
    let phase2_results: Vec<Result<(ResourceId, State), AppError>> =
        stream::iter(resolved_data_sources.iter())
            .map(|resource| {
                let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                let data_source_bindings = &data_source_bindings;
                let module_gate = &module_gate;
                async move {
                    let mut state = read_data_source_with_retry(
                        provider_ref,
                        resource,
                        data_source_bindings,
                        module_gate,
                        ctx.factories(),
                        ctx.schemas(),
                    )
                    .await?;
                    if let Some(deps) = dep_bindings {
                        state.dependency_bindings = deps;
                    }
                    progress.finish();
                    Ok((resource.id.clone(), state))
                }
            })
            .buffer_unordered(5)
            .collect()
            .await;
    for result in phase2_results {
        let (id, state) = result?;
        current_states.insert(id, state);
    }

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
    // quartet runs through `expand_refresh_and_lift_states` so the
    // apply and refresh paths cannot diverge on these phases. The
    // constructor is the only way to obtain an `ExpandedRefreshState`
    // — leaving a phase out becomes a compile error.
    let moved_targets: HashSet<ResourceId> = moved_pairs
        .iter()
        .map(|(_, to)| to.as_inner().clone())
        .collect();
    let crate::wiring::ExpandedRefreshState {
        sorted_resources: resorted,
        residual_deferred_for,
        deferred_create_targets,
        new_child_ids: _,
        refreshable_child_ids: _,
        // `printed_warnings` drives the plan path's spinner-bar-region
        // close (#3150); the apply path's post-refresh bar handling is
        // tracked separately in #3151 and does not consume it here.
        printed_warnings: _,
    } = crate::wiring::expand_refresh_and_lift_states(crate::wiring::ExpandRefreshAndLiftInputs {
        parsed,
        provider: &provider,
        sorted_resources: &sorted_resources,
        current_states: &mut current_states,
        orphans: &mut orphans,
        remote_bindings,
        wait_aliases: &wait_aliases,
        moved_targets: &moved_targets,
        already_refreshed: &orphan_refreshed_ids,
        state_file: &state_file,
        saved_dep_bindings: &HashMap::new(),
        saved_attrs: &saved_attrs,
        multi: &multi,
        schemas: ctx.schemas(),
    })
    .await?;
    sorted_resources = resorted;
    // Expansion borrows `parsed` immutably (expands a clone), so
    // `parsed.deferred_for_expressions` is NOT drained of resolved
    // loops the way the old `&mut self` call drained it. `print_plan`
    // must therefore receive this *residual* (still-unresolvable) list,
    // not `parsed.deferred_for_expressions` — otherwise a resolved loop
    // renders as both its materialized children AND a phantom
    // "deferred" entry. Mirrors the plan path's `ctx.residual_deferred_for`
    // (`commands/plan.rs`).

    // Data-source lifting stays inline: it operates on
    // `parsed.data_sources` (not `sorted_resources`), so it is
    // independent of the deferred-for expansion sequence the helper
    // above pins.
    carina_core::utils::lift_current_state_enum_leaves_for_data_sources(
        &mut current_states,
        &data_sources,
        ctx.schemas(),
    );

    // Snapshot each composition resource's *pre-resolve* attributes
    // (still carrying `ResourceRef`s) before the head-of-pipeline
    // resolver call below replaces them with pre-apply concrete
    // values. `finalize_apply`'s export resolution needs this
    // snapshot to re-resolve compositions against the post-apply state —
    // see #3169 / #3177. Without it, every composition ref is frozen at
    // the pre-apply value and `state.exports` captures stale data
    // after any Replace on a referenced managed resource.
    // carina#3181: composition resources live in `parsed.compositions`
    // as their own typed slice.
    let pre_resolve_compositions: Vec<carina_core::resource::Composition> =
        parsed.compositions.clone();

    let pre_apply_input_states = carina_core::resource::into_plan_input_map(
        current_states.clone(),
        ctx.schemas(),
        &sorted_resources,
    );
    let mut override_aware_resources = OverrideAwareResources::build(
        sorted_resources,
        state_file.as_ref(),
        carina_core::binding_index::PreApplyInputs {
            managed: &[],
            compositions: &pre_resolve_compositions,
            data_sources: &data_sources,
            current_states: &pre_apply_input_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
    )?;
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        state_file.as_ref(),
    );
    let unresolved_override_aware_resources = OverrideAwareResources::build(
        unresolved_parsed.resources.clone(), // allow: direct — parser-internal, pre-expansion
        state_file.as_ref(),
        carina_core::binding_index::PreApplyInputs {
            managed: &[],
            compositions: &pre_resolve_compositions,
            data_sources: &data_sources,
            current_states: &pre_apply_input_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
    )?;
    // Resolve and canonicalize only refresh-time data sources. Deferred
    // reads keep their original input refs so the plan/executor dependency
    // graph still contains the upstream create edge.
    let deferred_data_source_ids: HashSet<ResourceId> = data_sources
        .iter()
        .filter(|resource| deferred_data_source_reads.contains(&resource.id))
        .map(|resource| resource.id.clone())
        .collect();
    let mut data_sources_for_plan = prepare_data_sources_for_plan(
        &data_sources,
        &deferred_data_source_ids,
        override_aware_resources.bindings(),
        None,
    )?;
    let constraint_origin_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    // Run the normalization pipeline (same as plan path in wiring.rs).
    // `create_plan` resolves typed wait RHS values against target schemas.
    let wait_bindings = parsed.wait_bindings.clone();
    let preprocessor = crate::wiring::PlanPreprocessor::new(&provider, ctx);
    let preparation = preprocessor
        .prepare(
            &mut override_aware_resources,
            &constraint_origin_resources,
            &module_gate,
            &mut current_states,
            &mut orphans,
            raw_provider_configs,
            &mut data_sources_for_plan,
            &data_sources,
        )
        .await;
    if let Err(error) = preparation {
        let errors = match error {
            crate::wiring::PlanPreparationError::Plan(errors) => errors,
            crate::wiring::PlanPreparationError::Provider(error) => {
                return Err(AppError::Provider(error));
            }
        };
        let mut plan = Plan::new();
        for error in errors {
            plan.add_error(error);
        }
        render_plan_errors_and_abort(&plan)?;
        unreachable!("render_plan_errors_and_abort returns an error for an invalid plan");
    }
    let resolved_desired_ids = reconcile_late_anonymous_identities(
        ctx,
        LateAnonymousIdentityInputs {
            resources: &mut override_aware_resources,
            state_file: state_file.as_ref(),
            state_block_claims: &state_block_claims,
            providers: raw_provider_configs,
        },
    )?;
    crate::wiring::validate_plan_time_state_block_collisions(
        &resolved_desired_ids,
        &moved_pairs,
        &resolved_state_block_targets,
        &state_file,
    )?;
    let reconciled_provider_states = provider_states_for(
        &current_states,
        &orphans,
        override_aware_resources.resources(),
    );
    let reconciled_input_states = carina_core::resource::into_plan_input_map(
        reconciled_provider_states,
        ctx.schemas(),
        override_aware_resources.resources(),
    );
    override_aware_resources.rebuild_bindings_after_identity_reconciliation(
        RebuildBindingsInputs {
            compositions: &pre_resolve_compositions,
            data_sources: &data_sources,
            current_states: &reconciled_input_states,
            remote_bindings,
            wait_aliases: &wait_aliases,
        },
    );
    let paired_unresolved_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let resolved_resources =
        ResolvedResource::collect_resolved(override_aware_resources.resources().iter().cloned())?;
    let resolved_unresolved_resources =
        ResolvedResource::collect_resolved(paired_unresolved_resources.iter().cloned())?;
    let plan_input_states = carina_core::resource::into_plan_input_map(
        current_states.clone(),
        ctx.schemas(),
        override_aware_resources.resources(),
    );
    let directives_map = state_file
        .as_ref()
        .map(|sf| sf.build_directives())
        .unwrap_or_default();
    let schemas = ctx.schemas();
    let mut plan = create_plan_with_cascades(
        &resolved_resources,
        &resolved_unresolved_resources,
        &data_sources_for_plan,
        &provider,
        PlanPriorStates::new(&plan_input_states, &orphans),
        &directives_map,
        schemas,
        &saved_attrs,
        &prev_explicit,
        &wait_bindings,
    )?;
    crate::wiring::add_deposed_delete_effects(&mut plan, &state_file);
    block_deletes_on_prior_consumer_updates(&mut plan, &directives_map);

    // Add state block effects (import/removed/moved) to the plan.
    // carina#3329: resolve `import { id = "${…}|…" }` interpolations
    // against the same binding view the resource-attribute resolver
    // uses. The empty `unresolved_upstream_bindings` set here just
    // skips the plan-time "stamp upstream refs as
    // `(known after upstream apply: …)`" pass — apply does not need
    // that display marker. The hard-fail on a still-deferred
    // identifier is enforced one layer down in
    // `execute_import_effects` via [`carina_core::effect::resolve_import_identifier`],
    // which rejects any non-Concrete `Value` before it can reach the
    // provider's `read()`.
    let no_unresolved_upstreams: std::collections::HashSet<&str> = std::collections::HashSet::new();
    crate::wiring::add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        &state_file,
        &moved_pairs,
        override_aware_resources.resources(),
        override_aware_resources.bindings(),
        &no_unresolved_upstreams,
    );
    crate::wiring::add_deferred_create_effects(&mut plan, &deferred_create_targets);
    current_states = into_all_prior_states(orphans, current_states);

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
        .collect();

    render_plan_errors_and_abort(&plan)?;

    if can_use_export_only_fast_path(&plan, &deferred_data_source_reads) {
        // No mutating effects — the plan only holds `Read` (data-source
        // reads) or `Wait` effects, or is entirely empty. Either way no
        // resource apply pipeline needs to run; route through the
        // export-only fast path. Without this, a plan whose only
        // difference from state is an export expression that reads a
        // data source (a `Read` effect lands in the plan, but no
        // mutation does) silently falls through into the
        // resource-apply pipeline and the `Persisting N export
        // change(s) to state.` banner never prints (carina#3270).
        finalize_module_constraints(
            &pre_resolve_compositions,
            override_aware_resources.bindings(),
        )?;
        let checked_exports_pre_apply =
            crate::commands::plan::check_exports_pre_apply(CheckExportsPreApplyInput {
                export_params: &parsed.export_params,
                resources: &paired_unresolved_resources,
                compositions: &parsed.compositions,
                data_sources: &parsed.data_sources,
                current_states: &current_states,
                state_file: state_file.as_ref(),
                plan: &plan,
                schemas: ctx.schemas(),
                wait_aliases: &wait_aliases,
                provider_context,
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

        if export_changes.is_empty() {
            println!("{}", "No changes needed.".green());
            return Ok(None);
        }

        let historical_display =
            carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&plan);
        print_plan(
            &plan,
            DetailLevel::Full,
            &historical_display,
            Some(ctx.schemas()),
            &HashMap::new(),
            &export_changes,
            &residual_deferred_for,
            None,
            None,
        );

        let stdin = tokio::io::BufReader::new(tokio::io::stdin());
        if confirm_apply(stdin, cancel.clone(), auto_approve).await? == ApplyConfirmation::Cancelled
        {
            return Ok(None);
        }

        println!(
            "{}",
            format!(
                "Persisting {} export change(s) to state.",
                export_changes.len()
            )
            .cyan()
        );
        // No-op apply path: compositions were never resolved (no head-of-
        // pipeline pass ran). carina#3181: compositions live in
        // `parsed.compositions` and still carry the authored
        // `ResourceRef` snapshots — the shape post-apply export resolution expects
        // from the post-apply path.
        let pre_resolve_compositions_noop: Vec<carina_core::resource::Composition> =
            parsed.compositions.clone();
        persist_exports_only(
            backend,
            lock,
            state_file,
            &paired_unresolved_resources,
            &data_sources,
            &pre_resolve_compositions_noop,
            &checked_exports_pre_apply,
            ctx.schemas(),
            &wait_aliases,
            &current_states,
            provider_context,
        )
        .await?;
        return Ok(None);
    }

    // Build display-safe historical attributes from current and persisted state.
    let historical_display =
        collect_historical_display_attributes(&plan, &current_states, state_file.as_ref());

    let checked_exports_pre_apply =
        crate::commands::plan::check_exports_pre_apply(CheckExportsPreApplyInput {
            export_params: &parsed.export_params,
            resources: &paired_unresolved_resources,
            compositions: &parsed.compositions,
            data_sources: &parsed.data_sources,
            current_states: &current_states,
            state_file: state_file.as_ref(),
            plan: &plan,
            schemas: ctx.schemas(),
            wait_aliases: &wait_aliases,
            provider_context,
        })
        .map_err(AppError::Validation)?;
    let current_exports = state_file
        .as_ref()
        .map(|s| s.exports.clone())
        .unwrap_or_default();
    let export_changes =
        crate::commands::plan::compute_export_diffs(&checked_exports_pre_apply, &current_exports);
    print_plan(
        &plan,
        DetailLevel::Full,
        &historical_display,
        Some(ctx.schemas()),
        &moved_origins,
        &export_changes,
        &residual_deferred_for,
        Some(&prev_explicit),
        None,
    );

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    if confirm_apply(stdin, cancel.clone(), auto_approve).await? == ApplyConfirmation::Cancelled {
        return Ok(None);
    }

    let apply_phase_started = Instant::now();
    println!("{}", "Applying changes...".cyan().bold());
    println!();

    // Build unresolved resource map for re-resolution at apply time
    let unresolved_resources = override_aware_resources
        .unresolved_by_resolved_id_with_binding_sources(&unresolved_override_aware_resources)?;
    let mut bindings = override_aware_resources.bindings().clone();

    // `provider` is a `ProviderRouter`, which impls both `Provider` and
    // `ProviderNormalizer`; the same object is passed in both positions
    // so apply re-normalizes with exactly the plan-time normalizer
    // (carina#3060). They must stay the same object.
    let observer = observer_factory(&plan);
    let outcome = execute_effects_with_observer(
        &plan,
        &provider,
        &provider,
        raw_provider_configs,
        ctx.factories(),
        ctx.schemas(),
        &mut bindings,
        &mut current_states,
        &unresolved_resources,
        &data_sources,
        &pre_resolve_compositions,
        deferred_data_source_reads,
        cancel.clone(),
        parallelism,
        observer,
    )
    .await;
    let (mut result, cancelled) = split_execution_outcome(outcome);

    // Execute import effects: read imported resources from the provider
    execute_import_effects(&plan, &provider, &mut result, &cancel).await;

    // Execute remove and move effects (state-only, logged for user feedback)
    execute_state_only_effects(&plan, &mut result);
    let resources_finished = Instant::now();

    // Use override-aware resources (post-default_tags merge, post-canonicalize)
    // for state writeback so the per-resource `explicit` tree includes
    // provider-level default tags. Otherwise the next plan projects the
    // tags out of `current` and surfaces a spurious `tags: (none) → ...`
    // diff (refs awscc#206).
    let skipped_exports = finalize_after_execute(
        |persistence| {
            finalize_apply(
                FinalizeApplyInput {
                    result: &result,
                    state_file,
                    sorted_resources: &resolved_resources,
                    data_sources: &data_sources_for_plan,
                    current_states: &current_states,
                    plan: &plan,
                    backend,
                    lock,
                    schemas,
                    export_params: Some(&checked_exports_pre_apply),
                    provider_context,
                    wait_aliases: &wait_aliases,
                    pre_resolve_compositions: &pre_resolve_compositions,
                },
                persistence,
                None,
            )
        },
        cancelled,
        &cancel,
    )
    .await?;

    finish_apply(
        &result,
        skipped_exports,
        resources_finished.duration_since(apply_phase_started),
    )
}

fn ensure_saved_plan_backend_matches_current(
    plan_backend_config: Option<&carina_core::parser::BackendConfig>,
    current_backend_config: Option<&carina_core::parser::BackendConfig>,
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

impl SavedPlanResourceLike for Resource {
    fn id(&self) -> &ResourceId {
        &self.id
    }

    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }

    fn attributes(&self) -> &indexmap::IndexMap<String, Value> {
        &self.attributes
    }
}

impl SavedPlanResourceLike for DataSource {
    fn id(&self) -> &ResourceId {
        &self.id
    }

    fn binding(&self) -> Option<&str> {
        self.binding.as_deref()
    }

    fn attributes(&self) -> &indexmap::IndexMap<String, Value> {
        &self.attributes
    }
}

fn rehydrate_saved_resource_like<T, Rehydrate>(
    saved: &mut [T],
    candidates: &HashMap<ResourceId, T>,
    apply_target: ApplyTarget<'_>,
    rehydrate: Rehydrate,
) -> Result<(), AppError>
where
    T: SavedPlanResourceLike,
    Rehydrate: Fn(&T, &T) -> Result<T, carina_core::value::SecretRehydrationSiteError>,
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

fn matching_resource_like<'a, T>(saved: &T, fresh: &'a [T]) -> Option<&'a T>
where
    T: SavedPlanResourceLike,
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

fn import_identifiers_from_current_config(
    plan: &Plan,
    parsed: &carina_core::parser::InferredFile,
) -> HashMap<ResourceId, Value> {
    let mut identifiers = HashMap::new();
    let bindings = bootstrap_resolved_bindings(parsed);
    let unresolved_upstream_bindings = HashSet::new();
    for effect in plan.effects() {
        let carina_core::effect::Effect::Import { id, .. } = effect else {
            continue;
        };
        if let Some(value) = parsed.state_blocks.iter().find_map(|block| match block {
            carina_core::parser::StateBlock::Import { to, id: value }
                if crate::wiring::resolve_import_target(to, &parsed.resources) == *id =>
            {
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
/// persisting secret placeholders. This function deliberately builds only
/// normalizers; CRUD-capable providers are created after every desired secret
/// has been verified.
async fn prepare_saved_plan_secret_candidates(
    parsed: &mut carina_core::parser::InferredFile,
    state_file: &Option<StateFile>,
    base_dir: &Path,
    provider_context: &ProviderContext,
) -> Result<(ResolvedProviderInputs, WiringContext), AppError> {
    let mut candidate_state = state_file.clone();
    let PreparedPlanIdentityInputs {
        wiring,
        provider_inputs,
        state_blocks: _,
    } = prepare_plan_identity_and_provider_inputs(
        parsed,
        &mut candidate_state,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Strict,
    )
    .await?;
    let normalizers =
        create_normalizers_from_configs_with_ctx(&wiring, provider_inputs.configs()).await?;
    prepare_desired_side_for_plan(
        &mut parsed.resources,
        &mut parsed.data_sources,
        provider_inputs.configs().as_provider_configs(),
        &normalizers,
        &wiring,
    )
    .await
    .map_err(AppError::Provider)?;

    Ok((provider_inputs, wiring))
}

fn rehydrate_desired_plan_file(
    mut plan_file: PlanFile,
    parsed: &carina_core::parser::InferredFile,
    provider_inputs: &ResolvedProviderInputs,
    apply_target: ApplyTarget<'_>,
) -> Result<PlanFile, AppError> {
    let mut resource_candidates = HashMap::new();
    for saved in &plan_file.sorted_resources {
        if let Some(fresh) = matching_resource_like(saved, &parsed.resources) {
            resource_candidates.insert(saved.id.clone(), fresh.clone());
        }
    }
    let mut data_source_candidates = HashMap::new();
    for saved in &plan_file.data_sources {
        if let Some(fresh) = matching_resource_like(saved, &parsed.data_sources) {
            data_source_candidates.insert(saved.id.clone(), fresh.clone());
        }
    }
    let import_identifiers = import_identifiers_from_current_config(&plan_file.plan, parsed);

    plan_file.plan = carina_core::value::rehydrate_plan_desired_secrets(
        &plan_file.plan,
        &resource_candidates,
        &data_source_candidates,
        &import_identifiers,
        &parsed.deferred_for_expressions,
    )
    .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;

    rehydrate_saved_resource_like(
        &mut plan_file.sorted_resources,
        &resource_candidates,
        apply_target,
        carina_core::value::rehydrate_resource_secrets,
    )?;
    rehydrate_saved_resource_like(
        &mut plan_file.unresolved_resources,
        &resource_candidates,
        apply_target,
        carina_core::value::rehydrate_resource_secrets,
    )?;

    for composition in &mut plan_file.compositions {
        let has_placeholder = composition
            .signature
            .arguments
            .values()
            .any(|argument| carina_core::value::contains_redacted_secret(argument.value()))
            || composition.signature.attributes.values().any(|attribute| {
                carina_core::value::contains_redacted_secret(&attribute.to_value())
            });
        if !has_placeholder {
            continue;
        }
        let candidate = parsed
            .compositions
            .iter()
            .find(|candidate| candidate.id == composition.id)
            .or_else(|| {
                let binding = composition.binding.as_deref()?;
                parsed
                    .compositions
                    .iter()
                    .find(|candidate| candidate.binding.as_deref() == Some(binding))
            })
            .ok_or_else(|| {
                saved_plan_secret_shape_error(composition.id.to_string(), apply_target)
            })?;
        *composition = carina_core::value::rehydrate_composition_secrets(composition, candidate)
            .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
    }

    rehydrate_saved_resource_like(
        &mut plan_file.data_sources,
        &data_source_candidates,
        apply_target,
        carina_core::value::rehydrate_data_source_secrets,
    )?;
    rehydrate_saved_resource_like(
        &mut plan_file.data_source_origins,
        &data_source_candidates,
        apply_target,
        carina_core::value::rehydrate_data_source_secrets,
    )?;

    let fresh_provider_configs =
        carina_core::parser::SavedProviderConfigs::from(provider_inputs.configs());
    plan_file.provider_configs = plan_file
        .provider_configs
        .rehydrate_secrets(&fresh_provider_configs)
        .map_err(|error| saved_plan_secret_rehydration_error(error, apply_target))?;
    for (binding, attributes) in &mut plan_file.upstream_snapshot {
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

    Ok(plan_file)
}

pub async fn run_apply_from_plan(
    plan_path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
) -> Result<(), AppError> {
    run_apply_from_plan_with_observer_factory(
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
    plan_path: &Path,
    auto_approve: bool,
    lock: bool,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
    provider_context: &ProviderContext,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
) -> Result<(), AppError> {
    let bytes = fs::read(plan_path).map_err(|source| AppError::PlanFileRead {
        path: plan_path.to_path_buf(),
        source,
    })?;
    let header: PlanFileHeader =
        serde_json::from_slice(&bytes).map_err(|source| match source.classify() {
            serde_json::error::Category::Data => AppError::InvalidPlanFile {
                path: plan_path.to_path_buf(),
                source,
            },
            serde_json::error::Category::Syntax
            | serde_json::error::Category::Eof
            | serde_json::error::Category::Io => AppError::UnreadableJsonPlanFile {
                path: plan_path.to_path_buf(),
                source,
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
            source: <serde_json::Error as serde::de::Error>::custom(
                "expected a top-level JSON object",
            ),
        });
    }
    let replan_command = header
        .source_path
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .map(|source_path| saved_plan_replan_command(plan_path, Path::new(source_path)));

    // Validate version compatibility. Plan-file version 16 guarantees that
    // unquoted wait RHS identifiers use the typed enum representation; v15
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

    let plan_file: PlanFile =
        serde_json::from_slice(&bytes).map_err(|source| AppError::CorruptPlanFile {
            path: plan_path.to_path_buf(),
            replan_command,
            source,
        })?;
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
            .yellow()
        );
    }

    println!(
        "{}",
        format!(
            "Using saved plan from {} (created {})",
            plan_file.source_path, plan_file.timestamp
        )
        .cyan()
    );

    let base_dir = get_base_dir(&source_path);
    let inspected_plan = InspectedPlanFile::inspect(plan_file);
    let needs_secret_verification = inspected_plan.needs_secret_verification();
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
    let mut loaded_current_config = match loaded_current_config {
        Ok(config) => Some(config),
        Err(source) if needs_secret_verification => {
            return Err(saved_plan_secret_evaluation_error(
                AppError::Config(source),
                apply_target,
            ));
        }
        Err(_) => None,
    };
    let parsed_backend = loaded_current_config
        .as_ref()
        .map(|config| config.parsed.backend.clone());
    let mut current_parsed = if needs_secret_verification {
        let mut current_config = loaded_current_config
            .take()
            .expect("secret verification requires a loaded current configuration");
        let module_walk = crate::module_walk::ModuleWalk::load(&current_config.parsed, base_dir);
        let duplicate_errors = crate::commands::duplicate_declaration_errors(
            &current_config.duplicate_declarations,
            &module_walk,
        );
        if !duplicate_errors.is_empty() {
            return Err(saved_plan_secret_evaluation_error(
                crate::commands::collapse_errors(duplicate_errors),
                apply_target,
            ));
        }
        crate::commands::validate_and_resolve_with_config(
            &mut current_config.parsed,
            base_dir,
            false,
            &current_config.inference_errors,
            &current_config.duplicate_declarations,
        )
        .map_err(|source| saved_plan_secret_evaluation_error(source, apply_target))?;
        Some(current_config.parsed)
    } else {
        None
    };
    let current_backend = current_parsed
        .as_ref()
        .map(|parsed| parsed.backend.as_ref())
        .or_else(|| parsed_backend.as_ref().map(Option::as_ref));
    let backend_config = match current_backend {
        Some(current_backend) => current_backend,
        None => inspected_plan.backend_config(),
    };
    let verified_backend = verify_for_mutation(&source_path, backend_config, DriftCommand::Apply)?;
    // Preserve the established gate order: configuration-vs-lock drift
    // requires `init --migrate-state` first; only once that is clean can a
    // plan-vs-current-source mismatch ask for a new plan.
    if let Some(current_backend) = current_backend {
        ensure_saved_plan_backend_matches_current(
            inspected_plan.backend_config(),
            current_backend,
            apply_target,
        )?;
    }

    // Set up backend
    let backend: Box<dyn StateBackend> = verified_backend
        .resolve()
        .await
        .map_err(AppError::Backend)?;

    // Acquire lock (unless --lock=false)
    let lock_info: Option<LockInfo> = if lock {
        println!("{}", "Acquiring state lock...".cyan());
        let li = backend
            .acquire_lock("apply")
            .await
            .map_err(|error| map_lock_error(error, &source_path))?;
        println!("  {} Lock acquired", "✓".green());
        Some(li)
    } else {
        println!(
            "{}",
            "Warning: State locking is disabled. This is unsafe if others might run commands against the same state."
                .yellow()
                .bold()
        );
        None
    };

    let op_result = run_apply_from_plan_locked(
        inspected_plan,
        current_parsed.as_mut(),
        provider_context,
        auto_approve,
        backend.as_ref(),
        lock_info.as_ref(),
        apply_target,
        cancel.clone(),
        observer_factory,
        parallelism,
        accept_legacy_name_overrides,
    )
    .await
    .map_err(|error| error.with_project_dir(&source_path));

    // Always release lock if it was acquired
    if let Some(ref li) = lock_info {
        let release_result = release_lock_after_execute(backend.as_ref(), li, &cancel).await;

        if release_result.is_ok()
            && (op_result.is_ok() || matches!(op_result, Err(AppError::Interrupted)))
        {
            println!("  {} Lock released", "✓".green());
        }

        let timing = op_result?;
        release_result?;
        if let Some(elapsed) = timing {
            println!("{}", format_total_apply_line(elapsed));
        }
    } else {
        if let Some(timing) = op_result? {
            println!("{}", format_total_apply_line(timing));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_apply_from_plan_locked(
    inspected_plan: InspectedPlanFile,
    current_parsed: Option<&mut carina_core::parser::InferredFile>,
    provider_context: &ProviderContext,
    auto_approve: bool,
    backend: &dyn StateBackend,
    lock: Option<&LockInfo>,
    apply_target: ApplyTarget<'_>,
    cancel: ShutdownToken,
    observer_factory: &ObserverFactory<'_>,
    parallelism: NonZeroUsize,
    accept_legacy_name_overrides: bool,
) -> Result<Option<Duration>, AppError> {
    let project_dir = apply_target.source_dir();
    // Read current state and validate lineage. carina#3315: a
    // pending in-memory schema migration must be persisted under
    // the apply lock so the carina#3283 warning text matches
    // reality — but we run that *after* the saved-plan
    // lineage/serial check below, so the operator sees the original
    // on-disk serial (the one the saved plan was diffed against)
    // rather than a +1 bump caused by our own migration persist.
    let (mut state_file, pending_migration) =
        match backend.read_state().await.map_err(AppError::Backend)? {
            None => (None, None),
            Some(carina_state::LoadedState::Pristine(state)) => (Some(state), None),
            Some(carina_state::LoadedState::Migrated { state, info }) => (Some(state), Some(info)),
        };
    if let Some(state) = state_file.as_ref() {
        check_legacy_name_overrides(state, accept_legacy_name_overrides, apply_target)?;
    }

    if let Some(ref state) = state_file {
        // Validate state lineage
        if let Some(plan_lineage) = inspected_plan.state_lineage()
            && state.lineage != plan_lineage
        {
            return Err(AppError::Config(format!(
                "State lineage mismatch: plan was created for lineage '{}' but current state has '{}'",
                plan_lineage, state.lineage
            )));
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
                .yellow()
            );
        }
    }

    // Persist an in-memory schema migration after the saved-plan lineage and
    // serial checks, but before drift detection or secret verification can
    // return early (carina#3315). The migration is independent of plan values
    // and records only the state schema upgrade performed while reading.
    if pending_migration.is_some()
        && let Some(state) = state_file.as_mut()
    {
        if let Some(lk) = lock {
            save_state_locked(backend, lk, state).await?;
        } else {
            save_state_unlocked(backend, state).await?;
        }
    }

    let (desired_plan, prepared_wiring) = match inspected_plan {
        InspectedPlanFile::Verified(plan) => (DesiredPlanFile::Verified(plan), None),
        InspectedPlanFile::NeedsSecretVerification(plan) => {
            let parsed = current_parsed.ok_or_else(|| {
                saved_plan_secret_evaluation_error(
                    AppError::Config("the current configuration was unavailable".to_string()),
                    apply_target,
                )
            })?;
            let (provider_inputs, wiring) = prepare_saved_plan_secret_candidates(
                parsed,
                &state_file,
                project_dir,
                provider_context,
            )
            .await
            .map_err(|source| saved_plan_secret_evaluation_error(source, apply_target))?;
            (
                plan.verify_desired(parsed, &provider_inputs, apply_target)?,
                Some(wiring),
            )
        }
    };

    let resolved_provider_configs =
        ResolvedProviderConfigs::try_from(desired_plan.provider_configs().clone())
            .map_err(|source| AppError::SavedProviderConfig(Box::new(source)))?;
    let raw_provider_configs = resolved_provider_configs.as_provider_configs();

    // Desired/config secret verification is complete before a CRUD-capable
    // provider is constructed. Historical placeholders are verified by the
    // drift reads below before the executor gate is minted.
    let ctx = match prepared_wiring {
        Some(wiring) => wiring,
        None => {
            let (factories, _) = build_factories_from_providers(raw_provider_configs, project_dir)?;
            WiringContext::new(factories, raw_provider_configs)
        }
    };
    let provider =
        create_providers_from_configs_with_ctx(&ctx, &resolved_provider_configs, project_dir)
            .await?;

    // Drift detection: re-read actual infrastructure state and compare against planned states
    println!("{}", "Checking for infrastructure drift...".cyan());
    let historical_addresses = desired_plan.historical_resource_addresses();
    let drift_check = detect_drift(
        desired_plan.sorted_resources(),
        desired_plan.current_states(),
        &historical_addresses,
        &provider,
        apply_target,
    )
    .await?;

    if let Some(drift_messages) = drift_check.drift_messages() {
        println!();
        println!("{}", "Error: Infrastructure drift detected!".red().bold());
        println!(
            "{}",
            "The following resources have changed since the plan was created:".red()
        );
        println!();
        for msg in drift_messages {
            println!("{}", msg);
        }
        println!();
        println!("{}", drift_replan_hint(apply_target).yellow());
        return Err(AppError::Config(
            "Apply aborted due to infrastructure drift.".to_string(),
        ));
    }

    println!("  {} No drift detected.", "✓".green());

    let (mut actual_states, mut verified_planned_states) = drift_check.into_verified_states();

    let rehydrated_plan_file = desired_plan
        .verify_historical(
            &provider,
            &mut actual_states,
            &mut verified_planned_states,
            &historical_addresses,
            apply_target,
        )
        .await?;

    let plan = rehydrated_plan_file.plan();
    let sorted_resources = rehydrated_plan_file.sorted_resources();
    let resolved_sorted_resources =
        ResolvedResource::collect_resolved(sorted_resources.iter().cloned())?;
    let plan_compositions = rehydrated_plan_file.compositions();
    let plan_data_sources = rehydrated_plan_file.data_sources();
    let plan_data_source_origins: &[carina_core::resource::DataSource] =
        rehydrated_plan_file.data_source_origins();

    let planned_states: HashMap<ResourceId, State> = rehydrated_plan_file
        .current_states()
        .iter()
        .map(|entry| (entry.id.clone(), entry.state.clone()))
        .collect();
    let deferred_data_source_reads = deferred_data_source_reads_from_data_sources(
        plan_data_source_origins,
        sorted_resources,
        &planned_states,
    );

    // Use the actual states (freshly read) as current_states for apply
    let mut current_states = planned_states;

    render_plan_errors_and_abort(plan)?;

    if can_use_export_only_fast_path(plan, &deferred_data_source_reads) {
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
                binding: carina_core::parser::BindingName::new(wb.binding.clone()),
                target: carina_core::parser::BindingName::new(wb.target.clone()),
            })
            .collect();
        let terminal_input_states = carina_core::resource::into_plan_input_map(
            current_states.clone(),
            ctx.schemas(),
            sorted_resources,
        );
        let terminal_bindings =
            ResolvedBindings::pre_apply(carina_core::binding_index::PreApplyInputs {
                managed: sorted_resources,
                compositions: plan_compositions,
                data_sources: plan_data_sources,
                current_states: &terminal_input_states,
                remote_bindings: rehydrated_plan_file.upstream_snapshot(),
                wait_aliases: &terminal_wait_aliases,
            });
        finalize_module_constraints(plan_compositions, &terminal_bindings)?;
        println!("{}", "No changes needed.".green());
        return Ok(None);
    }

    // Build display-safe historical attributes from current and persisted state.
    let historical_display =
        collect_historical_display_attributes(plan, &current_states, state_file.as_ref());

    print_plan(
        plan,
        DetailLevel::Full,
        &historical_display,
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );

    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    if confirm_apply(stdin, cancel.clone(), auto_approve).await? == ApplyConfirmation::Cancelled {
        return Ok(None);
    }

    // Verify upstream-state bindings have not drifted since `carina
    // plan` ran (#2303). Re-load each upstream the plan declared and
    // compare against the persisted snapshot; if any binding's
    // attribute map disagrees, fail rather than silently mixing
    // plan-time and apply-time values during cascade re-resolution.
    let upstream_snapshot = rehydrated_plan_file.upstream_snapshot().clone();
    if !rehydrated_plan_file.upstream_sources().is_empty() {
        verify_upstream_snapshot(
            rehydrated_plan_file.upstream_sources(),
            &upstream_snapshot,
            apply_target,
        )
        .await?;
    }
    // Rebuild the wait passthrough aliases from the persisted
    // `(binding, target)` pairs so `apply --plan`'s cascade
    // re-resolution resolves `<wait-binding>.<attr>` exactly as the
    // plan path did (carina#3085). `PlanWaitBinding` (String) →
    // `WaitAliasSpec` (typed `BindingName`); the parser `WaitBinding`
    // is not serializable so it is reconstructed here, not deserialized.
    let wait_aliases: Vec<WaitAliasSpec> = rehydrated_plan_file
        .wait_bindings()
        .iter()
        .map(|wb| WaitAliasSpec {
            binding: carina_core::parser::BindingName::new(wb.binding.clone()),
            target: carina_core::parser::BindingName::new(wb.target.clone()),
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
    let pre_apply_input_states = carina_core::resource::into_plan_input_map(
        current_states.clone(),
        ctx.schemas(),
        sorted_resources,
    );
    let mut bindings = ResolvedBindings::pre_apply(carina_core::binding_index::PreApplyInputs {
        managed: sorted_resources,
        compositions: plan_compositions,
        data_sources: plan_data_sources,
        current_states: &pre_apply_input_states,
        remote_bindings: &upstream_snapshot,
        wait_aliases: &wait_aliases,
    });

    let apply_phase_started = Instant::now();
    println!("{}", "Applying changes...".cyan().bold());
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
    let observer = observer_factory(plan);
    let outcome = execute_rehydrated_saved_plan(
        &rehydrated_plan_file,
        &provider,
        &provider,
        raw_provider_configs,
        ctx.factories(),
        ctx.schemas(),
        &mut bindings,
        &mut current_states,
        &unresolved_resources,
        plan_data_source_origins,
        plan_compositions,
        deferred_data_source_reads,
        cancel.clone(),
        parallelism,
        observer,
    )
    .await;
    let (mut result, cancelled) = split_execution_outcome(outcome);

    // Execute import effects: read imported resources from the provider
    execute_import_effects(plan, &provider, &mut result, &cancel).await;

    // Execute remove and move effects (state-only, logged for user feedback)
    execute_state_only_effects(plan, &mut result);
    let resources_finished = Instant::now();

    // Reuse the exact factory/schema context that prepared and executed the
    // saved plan for write-only attribute persistence.
    let provider_context = ProviderContext::default();

    let skipped_exports = finalize_after_execute(
        |persistence| {
            finalize_apply(
                FinalizeApplyInput {
                    result: &result,
                    state_file,
                    sorted_resources: &resolved_sorted_resources,
                    data_sources: plan_data_sources,
                    current_states: &current_states,
                    plan,
                    backend,
                    lock,
                    schemas: ctx.schemas(),
                    // Saved plan files do not persist `export_params` today, so
                    // pass `None` to preserve `state.exports` rather than wipe it.
                    // Source-driven `carina apply` reconciles exports from the
                    // `.crn` directly (#2932).
                    export_params: None,
                    provider_context: &provider_context,
                    // No export resolution runs here (export_params is None), but
                    // the field is required; pass the reconstructed aliases so the
                    // value is correct if a future plan file persists export_params.
                    wait_aliases: &wait_aliases,
                    // Same rationale as `export_params: None`: no export
                    // resolution runs from the `apply --plan` path, so no
                    // pre-resolve composition snapshot is needed.
                    pre_resolve_compositions: &[],
                },
                persistence,
                Some(rehydrated_plan_file.planned_secret_hashes()),
            )
        },
        cancelled,
        &cancel,
    )
    .await?;

    finish_apply(
        &result,
        skipped_exports,
        resources_finished.duration_since(apply_phase_started),
    )
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

fn finish_apply(
    result: &ExecutionResult,
    skipped_exports: SkippedExports,
    elapsed: Duration,
) -> Result<Option<Duration>, AppError> {
    println!();
    let exit_code = apply_exit_code_for_counts(
        result.failure_count + result.skip_count,
        result.partial_count,
    );
    if exit_code == ApplyExitCode::Success && skipped_exports.is_empty() {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", result.success_count)
                .green()
                .bold()
        );
        return Ok(Some(elapsed));
    }

    let mut parts = vec![format!("{} succeeded", result.success_count)];
    if result.partial_count > 0 {
        parts.push(format!("{} partial", result.partial_count));
    }
    if result.failure_count > 0 {
        parts.push(format!("{} failed", result.failure_count));
    }
    if result.skip_count > 0 {
        parts.push(format!("{} skipped", result.skip_count));
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
        Err(AppError::Config(message))
    }
}

/// Prompt the user to confirm an apply. Shared between the resource-change and
/// export-only paths so both use identical wording and behavior.
pub(crate) async fn confirm_apply<R>(
    reader: R,
    cancel: ShutdownToken,
    auto_approve: bool,
) -> Result<ApplyConfirmation, AppError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    if auto_approve {
        return Ok(ApplyConfirmation::Confirmed);
    }

    println!(
        "{}",
        "Do you want to perform these actions?".yellow().bold()
    );
    println!(
        "  {}",
        "Carina will perform the actions described above. Type 'yes' to confirm.".yellow()
    );
    // The cursor is hidden command-wide (#3158); reveal it for the prompt
    // so the user does not type blind, and re-hide on scope exit.
    let input = {
        let _reveal = CursorReveal::new();
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let read_result = crate::signal::read_line_until_cancelled(reader, cancel).await;
        emit_newline_on_interrupt(&mut std::io::stdout(), &read_result);
        read_result?
    };

    if input.trim() != "yes" {
        println!();
        println!("{}", "Apply cancelled.".yellow());
        Ok(ApplyConfirmation::Cancelled)
    } else {
        println!();
        Ok(ApplyConfirmation::Confirmed)
    }
}

#[cfg(test)]
mod tests;
