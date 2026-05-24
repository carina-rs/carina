use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::effect::Effect;
use carina_core::parser::{BackendConfig, ProviderConfig, ProviderContext, UpstreamState};
use carina_core::plan::Plan;
use carina_core::resource::{
    ConcreteValue, DeferredValue, ManagedResource, ResourceId, State, Value,
};
use carina_core::value::{
    redact_secrets_in_plan, redact_secrets_in_resource, redact_secrets_in_state,
};
use carina_state::{
    BackendConfig as StateBackendConfig, StateBackend, StateFile, create_backend,
    create_local_backend, resolve_backend_anchored,
};

use super::validate_and_resolve_with_config;
use crate::DetailLevel;
use crate::commands::shared::state_writeback::apply_name_overrides;
use crate::display::{print_plan, refresh_plan_separator};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, create_plan_from_parsed_with_upstream,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
};

/// Saved plan file for `plan --out` / `apply plan.json`
#[derive(Debug, Serialize, Deserialize)]
pub struct PlanFile {
    /// Plan file format version
    pub version: u32,
    /// Carina version that created this plan
    pub carina_version: String,
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Original .crn path (informational)
    pub source_path: String,
    /// State lineage for drift detection
    pub state_lineage: Option<String>,
    /// State serial for drift detection
    pub state_serial: Option<u64>,
    /// Provider configurations
    pub provider_configs: Vec<ProviderConfig>,
    /// Backend configuration
    pub backend_config: Option<BackendConfig>,
    /// The plan (effects)
    pub plan: Plan,
    /// Resources sorted by dependencies (for post-apply state saving)
    pub sorted_resources: Vec<ManagedResource>,
    /// Virtual resources (module-call attribute containers) emitted
    /// by module expansion at plan time (carina#3248). Persisted so
    /// the saved-plan apply path builds the same `ResolvedBindings`
    /// view as the live-apply path: an attribute referencing
    /// `<module_instance>.<attr>` chains through the virtual's
    /// attribute map to the managed sibling literal that backs it.
    ///
    /// Pre-carina#3248 saved plans (version `3`) did not persist
    /// virtuals and apply-from-plan passed `&[]` into the executor —
    /// any virtual-rooted ref would survive resolution as a
    /// `ResourceRef` and fail-fast at the executor's
    /// `assert_fully_resolved` check, or produce a spurious diff if
    /// it reached the differ (carina#3246).
    ///
    /// Empty when the configuration declares no module calls /
    /// `attributes { ... }` blocks.
    #[serde(default)]
    pub virtual_resources: Vec<carina_core::resource::VirtualResource>,
    /// Data sources (`let x = read aws.iam.user { ... }`) emitted by
    /// module expansion at plan time (carina#3248). Persisted so the
    /// saved-plan apply path can re-create the same unified
    /// `ResolvedBindings` view as the live-apply path: a managed
    /// attribute referencing `<read_binding>.<attr>` resolves through
    /// the data source's attribute map.
    ///
    /// Pre-carina#3248 saved plans did not persist data sources
    /// separately (the field was missing); a managed→data-source ref
    /// would have left a `ResourceRef` unresolved at apply time. The
    /// typed-split (#3181) moved data sources out of
    /// `sorted_resources`, so persisting them explicitly is the only
    /// way to keep the saved-plan apply path consistent with the
    /// live-apply path.
    ///
    /// Empty when the configuration declares no `read`-bound
    /// bindings.
    #[serde(default)]
    pub data_sources: Vec<carina_core::resource::DataSource>,
    /// Current states (for binding_map + state saving)
    pub current_states: Vec<CurrentStateEntry>,
    /// `upstream_state` bindings as resolved at plan time (#2303).
    ///
    /// Persisted so `apply --plan` can verify the upstream values have
    /// not drifted between plan and apply. If any binding here disagrees
    /// with the freshly-loaded upstream view at apply time, the apply
    /// fails with a structured error rather than silently mixing
    /// plan-time and apply-time values during cascade re-resolution.
    ///
    /// Empty when the configuration declares no `upstream_state` blocks.
    pub upstream_snapshot: HashMap<String, HashMap<String, Value>>,
    /// `upstream_state` block sources (binding name → directory) as
    /// declared in the original `.crn` config (#2303). Persisted so
    /// `apply --plan` can re-load each upstream and compare against
    /// `upstream_snapshot`.
    pub upstream_sources: Vec<UpstreamSource>,
    /// `wait` binding → target binding pairs as declared in the
    /// original `.crn` config (carina#3085). Persisted so `apply
    /// --plan`'s cascade re-resolution can rebuild the wait
    /// passthrough aliases (`<wait-binding>.<attr>` →
    /// `<target>.<attr>`) without the parser `WaitBinding` (which is
    /// not `Serialize` — same constraint that motivated
    /// [`UpstreamSource`]). Only the `(binding, target)` pair is
    /// persisted: the `until` predicate / timeout / `depends_on` are
    /// effect-layer concerns already encoded in the serialized `plan`'s
    /// `Effect::Wait`, not needed to rebuild the value-layer alias.
    /// Empty when the configuration declares no `wait` bindings.
    #[serde(default)]
    pub wait_bindings: Vec<PlanWaitBinding>,
}

/// Serializable `(binding, target)` pair for a `wait` declaration —
/// the value-layer half of a wait binding (carina#3085). The parser
/// `carina_core::parser::WaitBinding` is not `Serialize`/`Deserialize`
/// (pulling serde into `carina-core::parser::ast` has wider blast
/// radius than warranted), so the plan file persists this minimal
/// projection, mirroring [`UpstreamSource`]'s rationale. Converted to
/// `carina_core::binding_index::WaitAliasSpec` at apply-from-plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWaitBinding {
    pub binding: String,
    pub target: String,
}

/// Serializable representation of an `upstream_state` declaration's
/// source directory — needed because `parser::ast::UpstreamState` is
/// not `Serialize`/`Deserialize` and pulling a `serde` derive into
/// `carina-core::parser::ast` would have wider blast radius than this
/// PR wants. Constructed once at plan time and consumed at
/// apply-from-plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamSource {
    pub binding: String,
    pub source: std::path::PathBuf,
}

/// Entry for serializing current resource states
#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentStateEntry {
    pub id: ResourceId,
    pub state: State,
}

fn build_plan_file<E>(
    path: &Path,
    parsed: &carina_core::parser::File<E>,
    state_file: &Option<StateFile>,
    ctx: &crate::wiring::PlanContext,
) -> Result<PlanFile, carina_core::value::SerializationError> {
    Ok(PlanFile {
        // carina#3248: bumped 3→4 — saved plans now persist
        // `virtual_resources` so the saved-plan apply path can rebuild
        // the same `ResolvedBindings` view as the live-apply path.
        // Older plans (version `3` and below) are rejected with a
        // clear message pointing the user at re-running `plan`.
        version: 4,
        carina_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        source_path: path.display().to_string(),
        state_lineage: state_file.as_ref().map(|s| s.lineage.clone()),
        state_serial: state_file.as_ref().map(|s| s.serial),
        provider_configs: parsed.providers.clone(),
        backend_config: parsed.backend.clone(),
        plan: redact_secrets_in_plan(&ctx.plan)?,
        sorted_resources: ctx
            .sorted_resources
            .iter()
            .map(redact_secrets_in_resource)
            .collect::<Result<Vec<_>, _>>()?,
        virtual_resources: parsed
            .virtual_resources
            .iter()
            .map(carina_core::value::redact_secrets_in_virtual)
            .collect::<Result<Vec<_>, _>>()?,
        data_sources: parsed
            .data_sources
            .iter()
            .map(carina_core::value::redact_secrets_in_data_source)
            .collect::<Result<Vec<_>, _>>()?,
        current_states: ctx
            .current_states
            .iter()
            .map(|(id, state)| {
                Ok::<_, carina_core::value::SerializationError>(CurrentStateEntry {
                    id: id.clone(),
                    state: redact_secrets_in_state(state)?,
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        upstream_snapshot: ctx.upstream_snapshot.clone(),
        upstream_sources: parsed
            .upstream_states
            .iter()
            .map(|us| UpstreamSource {
                binding: us.binding.clone(),
                source: us.source.clone(),
            })
            .collect(),
        wait_bindings: parsed
            .wait_bindings
            .iter()
            .map(|wb| PlanWaitBinding {
                binding: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
            })
            .collect(),
    })
}

/// Format a `SerializationError` from `build_plan_file` into the
/// CLI's "cannot save plan" diagnostic, suggesting the caller drop
/// `flag` (e.g. `--json`, `--out`) to fall back to display-only.
fn format_plan_save_error(e: &carina_core::value::SerializationError, flag: &str) -> String {
    match e {
        carina_core::value::SerializationError::UnknownNotAllowed { reason, .. } => format!(
            "Cannot save plan: it depends on values that are not yet \
             applied ({reason}). Apply the upstream module(s) first, \
             or rerun without {flag}."
        ),
        _ => e.to_string(),
    }
}

/// A point-in-time fingerprint of the state file captured immediately
/// after `plan` first reads state (T0). `plan` takes no state lock
/// (issue #3111: a lock on a read-only operation is overkill and would
/// serialize concurrent `plan`s), so a concurrent `apply`/`destroy`
/// can mutate state between this snapshot and when the plan is
/// displayed. Comparing this snapshot against a re-read at T1 lets
/// `plan` warn that its output may be stale (TOCTOU drift detection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateSnapshot {
    /// `StateFile::serial` — monotonically increasing per state write.
    serial: u64,
    /// `StateFile::lineage` — changes only if the state was recreated
    /// from scratch (e.g. a destroy + fresh apply, or `state` surgery).
    lineage: String,
}

impl StateSnapshot {
    /// Capture the snapshot from the state read at T0. `None` when no
    /// state exists yet (first-ever plan): there is nothing a
    /// concurrent writer could make stale, so drift detection is moot.
    pub(crate) fn capture(state: Option<&StateFile>) -> Option<Self> {
        state.map(|s| Self {
            serial: s.serial,
            lineage: s.lineage.clone(),
        })
    }
}

/// How the state changed between the T0 snapshot and the T1 re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateDrift {
    /// Same lineage, higher serial: a concurrent `apply`/`destroy`
    /// wrote state while this `plan` was computing its diff.
    SerialAdvanced { from: u64, to: u64 },
    /// Lineage changed: the state was recreated entirely (destroy +
    /// fresh apply, or `carina state` surgery) under this `plan`.
    LineageChanged { from: String, to: String },
    /// State existed at T0 but is gone at T1 (deleted concurrently).
    StateRemoved,
}

impl StateDrift {
    /// User-facing warning line explaining the plan may be stale.
    pub(crate) fn warning(&self) -> String {
        let detail = match self {
            StateDrift::SerialAdvanced { from, to } => {
                format!("state serial advanced {from} -> {to}")
            }
            StateDrift::LineageChanged { from, to } => {
                format!("state lineage changed {from} -> {to}")
            }
            StateDrift::StateRemoved => "state was removed".to_string(),
        };
        format!(
            "Warning: state changed during plan ({detail}); a concurrent \
             apply/destroy ran while this plan was being computed. The \
             plan output may be stale — re-run `carina plan`."
        )
    }
}

/// Compare the T0 snapshot against the state re-read at T1 (just
/// before plan display) and classify any drift.
///
/// `before` is `None` only when no state existed at T0, in which case
/// nothing could have gone stale — return `None`. A lineage change is
/// reported in preference to a serial change because it is the
/// stronger signal (the entire state was replaced).
pub(crate) fn detect_state_drift(
    before: Option<&StateSnapshot>,
    after: Option<&StateFile>,
) -> Option<StateDrift> {
    let before = before?;
    match after {
        None => Some(StateDrift::StateRemoved),
        Some(after) => {
            if after.lineage != before.lineage {
                Some(StateDrift::LineageChanged {
                    from: before.lineage.clone(),
                    to: after.lineage.clone(),
                })
            } else if after.serial != before.serial {
                Some(StateDrift::SerialAdvanced {
                    from: before.serial,
                    to: after.serial,
                })
            } else {
                None
            }
        }
    }
}

/// Test-only rendezvous between `plan`'s T0 state read and its T1
/// re-read. Production builds never set the env var, so this is a
/// no-op (one failed `getenv`); when set by the drift e2e test it is a
/// file handshake — not a sleep — so the test can place a concurrent
/// writer *deterministically* in the T0..T1 window (a fixed sleep
/// would race the binary's own startup cost and flake). The binary
/// signals "T0 captured" by creating `<dir>/t0_done`, then blocks
/// until the test creates `<dir>/proceed`. The wait is bounded so a
/// broken test cannot hang the binary forever. It has no effect on
/// the concurrency contract.
async fn drift_detection_test_barrier() {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
    // Matches the test side's t0_done tolerance. Generous because the
    // repo runs process-per-test under heavily parallel nextest, where
    // the test thread can be starved between observing t0_done and
    // creating proceed; a tight bound here would flake. Unbounded is
    // unnecessary (a runaway test is already capped by nextest's slow
    // timeout) and this is test-only anyway, so it cannot hang prod.
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    let Ok(dir) = std::env::var("CARINA_TEST_PLAN_DRIFT_HANDSHAKE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::write(dir.join("t0_done"), b"");
    let proceed = dir.join("proceed");
    let deadline = tokio::time::Instant::now() + MAX_WAIT;
    while !proceed.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_plan(
    path: &Path,
    out: Option<&Path>,
    detail: DetailLevel,
    tui: bool,
    refresh: bool,
    json: bool,
    reconfigure: bool,
    provider_context: &ProviderContext,
) -> Result<bool, AppError> {
    let mut parsed = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?
    .parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve_with_config(&mut parsed, base_dir, false)?;

    // Detect backend reconfiguration before touching any state
    crate::commands::check_backend_lock(base_dir, parsed.backend.as_ref(), reconfigure)?;

    // Check for backend configuration and load state
    // Use local backend by default if no backend is configured
    let mut will_create_state_bucket = false;
    let mut state_bucket_name = String::new();
    let mut state_file: Option<StateFile> = None;

    let plan_backend: Box<dyn StateBackend> = if let Some(config) = parsed.backend.as_ref() {
        let state_config = StateBackendConfig::from(config);
        let backend = create_backend(&state_config)
            .await
            .map_err(AppError::Backend)?;

        let bucket_exists = backend.bucket_exists().await.map_err(AppError::Backend)?;

        if bucket_exists {
            // Try to load state from backend
            state_file = backend.read_state().await.map_err(AppError::Backend)?;
        } else {
            // Check if there's a matching s3_bucket resource defined
            let bucket_name = config
                .attributes
                .get("bucket")
                .and_then(|v| match v {
                    Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
                    _ => None,
                })
                .ok_or("Backend bucket name not specified")?;

            let backend_resource_type = backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            #[rustfmt::skip]
            let has_bucket_resource = parsed.resources.iter().any(|r| { // allow: direct — plan-time reconciliation
                r.id.resource_type == backend_resource_type
                    && r.attributes
                        .get("bucket")
                        .is_some_and(|v| matches!(v, Value::Concrete(ConcreteValue::String(s)) if s == &bucket_name))
            });

            if !has_bucket_resource {
                let auto_create = config
                    .attributes
                    .get("auto_create")
                    .and_then(|v| match v {
                        Value::Concrete(ConcreteValue::Bool(b)) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(true);

                if auto_create {
                    will_create_state_bucket = true;
                    state_bucket_name = bucket_name;
                } else {
                    return Err(AppError::Config(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    )));
                }
            }
        }
        backend
    } else {
        // Use local backend by default
        let backend = create_local_backend();
        state_file = backend.read_state().await.map_err(AppError::Backend)?;
        backend
    };

    // T0 fingerprint; see `StateSnapshot` for the TOCTOU rationale.
    let state_snapshot_t0 = StateSnapshot::capture(state_file.as_ref());

    // Show bootstrap plan if needed
    if will_create_state_bucket {
        let backend_provider = plan_backend
            .provider_name()
            .ok_or("Backend does not specify a provider name")?;
        let backend_resource_type = plan_backend
            .resource_type()
            .ok_or("Backend does not specify a resource type")?;
        println!("{}", "Bootstrap Plan:".cyan().bold());
        println!(
            "  {} {} (state bucket)",
            "+".green(),
            format!(
                "{}.{}.{}",
                backend_provider, backend_resource_type, state_bucket_name
            )
            .green()
        );
        println!(
            "  {} ManagedResource definition will be added to .crn file",
            "→".cyan()
        );
        println!();
    }

    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir);
    let wiring = WiringContext::new(factories);
    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.name.clone())
                    .collect()
            },
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(&wiring, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached state (--refresh=false). Plan may not reflect actual infrastructure.".yellow()
        );
    }

    let mut cycle_guard = seed_cycle_guard(base_dir);
    // #2366: plan tolerates missing upstream state; apply still strict.
    let remote_bindings = load_upstream_states(
        &parsed.upstream_states,
        base_dir,
        provider_context,
        &mut cycle_guard,
        UpstreamMissingStatePolicy::Lenient,
    )
    .await?;

    // carina#3182: substitute `upstream.<attr>` and other binding refs
    // inside `provider.attributes` (e.g. `assume_role.role_arn`) before
    // the provider attributes cross the WASM boundary in
    // `create_provider`. Parse leaves these refs in place; only here
    // (post-`load_upstream_states`) do we have the upstream values.
    carina_core::parser::resolve_provider_attributes_with_remote(
        &mut parsed,
        &remote_bindings,
        provider_context,
    )
    .map_err(|e| AppError::Config(format!("Provider attribute resolution error: {}", e)))?;

    // carina#3132: deferred-for expansion runs inside
    // `create_plan_from_parsed_with_upstream` (post-refresh), which also
    // prints post-expansion warnings and returns the still-unresolved
    // loops via `ctx.residual_deferred_for`.
    let ctx = create_plan_from_parsed_with_upstream(
        &parsed,
        &state_file,
        refresh,
        &remote_bindings,
        base_dir,
    )
    .await?;
    let has_changes = ctx.plan.mutation_count() > 0;

    // TOCTOU drift detection (#3111). `plan` took no state lock, so a
    // concurrent apply/destroy may have written state while the diff
    // above was being computed. Re-read state now and compare against
    // the T0 fingerprint; warn (do not fail) if it moved — the plan is
    // a prediction, and apply re-locks + recomputes before mutating.
    //
    // Skipped entirely when no state existed at T0 (first-ever plan, or
    // a bootstrap run that will create the backend): there is no
    // baseline, so nothing could have gone stale and even a re-read
    // *failure* is not worth a "may be stale" warning.
    if let Some(snapshot_t0) = state_snapshot_t0.as_ref() {
        drift_detection_test_barrier().await;
        match plan_backend.read_state().await {
            Ok(state_t1) => {
                if let Some(drift) = detect_state_drift(Some(snapshot_t0), state_t1.as_ref()) {
                    eprintln!("{}", drift.warning().yellow());
                }
            }
            // A failed re-read is not fatal: the plan already computed
            // against the T0 snapshot. Surface it so the user knows
            // drift could not be checked, but let the plan print.
            Err(e) => {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: could not re-read state to check for \
                         concurrent changes ({e}); plan output may be stale."
                    )
                    .yellow()
                );
            }
        }
    }

    // Check for prevent_destroy violations
    if ctx.plan.has_errors() {
        for err in ctx.plan.errors() {
            eprintln!("{} {}", "Error:".red().bold(), err);
        }
        return Err(AppError::Validation(format!(
            "{} resource(s) have prevent_destroy set and cannot be deleted or replaced",
            ctx.plan.errors().len()
        )));
    }

    // Build delete attributes map from current states for display
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = ctx
        .plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let Effect::Delete { id, .. } = e {
                ctx.current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();

    if json {
        // `build_plan_file` propagates `SerializationError` so the user
        // sees an actionable diagnostic (which upstream / for-binding
        // is unresolved) instead of a panic backtrace.
        let plan_file = build_plan_file(path, &parsed, &state_file, &ctx)
            .map_err(|e| format_plan_save_error(&e, "--json"))?;
        let json_str = serde_json::to_string_pretty(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        println!("{}", json_str);
    } else if tui {
        carina_tui::run(&ctx.plan, wiring.schemas())
            .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
    } else {
        // Resolve export values for display
        let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
            .wait_bindings
            .iter()
            .map(carina_core::binding_index::WaitAliasSpec::from)
            .collect();
        let resolved_exports = resolve_export_values_for_display(
            &parsed.export_params,
            &ctx.sorted_resources,
            &parsed.virtual_resources,
            &parsed.data_sources,
            &ctx.current_states,
            &export_wait_aliases,
        );
        let current_exports = state_file
            .as_ref()
            .map(|s| s.exports.clone())
            .unwrap_or_default();
        let export_changes = compute_export_diffs(&resolved_exports, &current_exports);
        // Separate the refresh-progress block (printed above when `refresh`)
        // from the plan's terminal section so they don't read as a run-on
        // (#3148).
        print!("{}", refresh_plan_separator(refresh));
        print_plan(
            &ctx.plan,
            detail,
            &delete_attributes,
            Some(wiring.schemas()),
            &ctx.moved_origins,
            &export_changes,
            &ctx.residual_deferred_for,
            Some(&ctx.prev_explicit),
        );
    }

    // Save plan to file if --out was specified
    if let Some(out_path) = out {
        let plan_file = build_plan_file(path, &parsed, &state_file, &ctx)
            .map_err(|e| format_plan_save_error(&e, "--out"))?;
        let json_out = carina_core::utils::pretty_with_newline(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        fs::write(out_path, json_out).map_err(|e| format!("Failed to write plan file: {}", e))?;

        println!();
        println!(
            "{}",
            format!("Plan saved to {}", out_path.display())
                .green()
                .bold()
        );
        println!(
            "{}",
            format!(
                "To apply this plan, run: carina apply {}",
                out_path.display()
            )
            .cyan()
        );
    }

    Ok(has_changes)
}

/// Load remote state files and build binding maps for reference resolution.
///
/// For each `remote_state` block, reads the referenced state file and builds a
/// map of resource bindings to their attributes. The result maps each remote_state
/// binding name to a `HashMap<String, Value>` where keys are resource binding names
/// and values are `Value::Concrete(ConcreteValue::Map)` of that resource's attributes.
/// Resolve export value expressions for plan display.
pub(crate) fn resolve_export_values_for_display(
    export_params: &[carina_core::parser::InferredExportParam],
    resources: &[ManagedResource],
    virtuals: &[carina_core::resource::VirtualResource],
    data_sources: &[carina_core::resource::DataSource],
    current_states: &HashMap<ResourceId, State>,
    wait_aliases: &[carina_core::binding_index::WaitAliasSpec],
) -> Vec<carina_core::parser::InferredExportParam> {
    // carina#3248: build the unified pre-apply bindings view so an
    // export referencing `<module_instance>.<attr>` chains through the
    // virtual to the managed sibling literal (carina#3246).
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: resources,
            virtuals,
            data_sources,
            current_states,
            remote_bindings: &HashMap::new(),
            wait_aliases,
        },
    );

    export_params
        .iter()
        .map(|param| {
            let resolved_value = param
                .value
                .as_ref()
                .map(|v| resolve_export_value(v, &bindings));
            carina_core::parser::InferredExportParam {
                name: param.name.clone(),
                type_expr: param.type_expr.clone(),
                value: resolved_value,
            }
        })
        .collect()
}

/// Resolve a single export value, handling both ResourceRef and dot-notation strings.
pub(crate) fn resolve_export_value(
    value: &Value,
    bindings: &carina_core::binding_index::ResolvedBindings,
) -> Value {
    use carina_core::resolver::resolve_ref_value;

    match value {
        Value::Deferred(DeferredValue::ResourceRef { .. }) => {
            resolve_ref_value(value, bindings).unwrap_or_else(|_| value.clone())
        }
        // Cross-file: "binding.attr" parsed as String instead of ResourceRef
        Value::Concrete(ConcreteValue::String(s)) if s.contains('.') && !s.contains(' ') => {
            let parts: Vec<&str> = s.splitn(2, '.').collect();
            if parts.len() == 2
                && let Some(attrs) = bindings.get(parts[0])
                && let Some(resolved) = attrs.get(parts[1])
            {
                return resolved.clone();
            }
            value.clone()
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let resolved: Vec<Value> = items
                .iter()
                .map(|item| resolve_export_value(item, bindings))
                .collect();
            Value::Concrete(ConcreteValue::List(resolved))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let resolved: indexmap::IndexMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), resolve_export_value(v, bindings)))
                .collect();
            Value::Concrete(ConcreteValue::Map(resolved))
        }
        _ => value.clone(),
    }
}

/// Represents a change to an export value between current state and desired.
pub enum ExportChange {
    Added {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        new_value: Value,
    },
    Modified {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        old_json: serde_json::Value,
        new_value: Value,
    },
    Removed {
        name: String,
        old_json: serde_json::Value,
    },
}

impl ExportChange {
    pub fn name(&self) -> &str {
        match self {
            ExportChange::Added { name, .. }
            | ExportChange::Modified { name, .. }
            | ExportChange::Removed { name, .. } => name,
        }
    }
}

/// Compute the set of export changes by comparing desired (resolved) exports
/// against current state-recorded exports.
///
/// `resolved_params` contains the desired export values resolved against
/// current resource states. `current_exports` is the JSON-serialized map
/// from `StateFile.exports`.
pub fn compute_export_diffs(
    resolved_params: &[carina_core::parser::InferredExportParam],
    current_exports: &HashMap<String, serde_json::Value>,
) -> Vec<ExportChange> {
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for param in resolved_params {
        seen.insert(param.name.clone());
        let Some(ref value) = param.value else {
            continue;
        };
        // Unknown sentinels carry no usable type information for the
        // plan display; surface as `None` so the diff prints without
        // a type tag.
        let type_expr = param.type_expr.clone().into_known();
        // Plan display tolerates `Value::Deferred(DeferredValue::Unknown)` in the export rhs
        // (the deferred-for body will resolve it later). Map both the
        // skip variants and the Unknown error to `None`; the diff
        // surfaces as a "value not yet known" change without a JSON.
        let new_json = crate::commands::shared::state_writeback::dsl_value_to_json(value)
            .ok()
            .flatten();
        match (current_exports.get(&param.name), new_json) {
            (None, _) => changes.push(ExportChange::Added {
                name: param.name.clone(),
                type_expr: type_expr.clone(),
                new_value: value.clone(),
            }),
            (Some(old), Some(new)) if old == &new => {
                // unchanged — skip
            }
            (Some(old), _) => changes.push(ExportChange::Modified {
                name: param.name.clone(),
                type_expr,
                old_json: old.clone(),
                new_value: value.clone(),
            }),
        }
    }

    // Removed: exports in state but not in desired params
    for (name, old) in current_exports {
        if !seen.contains(name) {
            changes.push(ExportChange::Removed {
                name: name.clone(),
                old_json: old.clone(),
            });
        }
    }

    changes.sort_by(|a, b| a.name().cmp(b.name()));
    changes
}

/// Seed a cycle guard with the caller's own base directory so that a chain
/// ending back at the root is detected as a cycle.
pub(crate) fn seed_cycle_guard(base_dir: &Path) -> HashSet<PathBuf> {
    let mut guard = HashSet::new();
    if let Ok(abs) = base_dir.canonicalize() {
        guard.insert(abs);
    }
    guard
}

/// How a missing upstream state file is handled while loading upstreams.
/// `apply` and recursive walks for cycle detection use [`Strict`] (error);
/// `plan` uses [`Lenient`] so a missing state demotes to a warning and
/// the upstream binding is recorded with no values (#2366).
///
/// [`Strict`]: UpstreamMissingStatePolicy::Strict
/// [`Lenient`]: UpstreamMissingStatePolicy::Lenient
#[derive(Clone, Copy)]
pub(crate) enum UpstreamMissingStatePolicy {
    Strict,
    Lenient,
}

/// Resolve and read each upstream's published exports by parsing its source
/// directory, deriving its backend, and pulling the state through that backend.
///
/// `cycle_guard` holds canonicalized absolute paths of directories currently
/// being resolved. An upstream whose source canonicalizes to a path already in
/// the guard is a cycle (A → B → A) and produces an error naming the path.
///
/// `policy` controls only the "state file is missing" case and propagates
/// through the recursive cycle-walk so a chained upstream (A → B → C) that
/// is partially unapplied still produces warnings + display markers under
/// `Lenient` instead of a hard error. Cycle detection, source-path
/// resolution failures, upstream `.crn` parse errors, and backend I/O
/// errors remain hard errors regardless — they indicate structural
/// problems the user must fix before plan output can mean anything.
pub(crate) async fn load_upstream_states(
    upstream_states: &[UpstreamState],
    base_dir: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
    policy: UpstreamMissingStatePolicy,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut result = HashMap::new();

    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize().map_err(|e| {
            AppError::Config(format!(
                "upstream_state '{}': cannot resolve source '{}': {}",
                us.binding,
                us.source.display(),
                e
            ))
        })?;

        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state '{}': cycle detected at {}",
                us.binding,
                source_abs.display()
            )));
        }

        let backend_result =
            build_upstream_backend(us, &source_abs, provider_context, cycle_guard).await;
        cycle_guard.remove(&source_abs);
        let backend = backend_result?;

        let state_file = backend.read_state().await.map_err(AppError::Backend)?;
        let bindings = match (state_file, policy) {
            (Some(sf), _) => sf.build_remote_bindings(),
            (None, UpstreamMissingStatePolicy::Strict) => {
                return Err(AppError::Config(format!(
                    "upstream_state '{}': no state found at {}",
                    us.binding,
                    source_abs.display()
                )));
            }
            (None, UpstreamMissingStatePolicy::Lenient) => {
                let msg = format!(
                    "Warning: upstream_state '{}': no state found at {}; \
                     dependent values will display as `(known after upstream apply: ...)`",
                    us.binding,
                    source_abs.display()
                );
                eprintln!("{}", msg.yellow());
                HashMap::new()
            }
        };
        result.insert(us.binding.clone(), bindings);
    }

    Ok(result)
}

/// Resolve an upstream's backend: parse its `.crn`, walk its own upstream
/// chain (cycle detection only — never state I/O — see
/// carina-rs/carina#2608), then build the backend honoring local-path
/// anchoring. Shared between strict and lenient upstream loaders.
async fn build_upstream_backend(
    us: &UpstreamState,
    source_abs: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
) -> Result<Box<dyn StateBackend>, AppError> {
    let loaded = load_configuration_with_config(
        source_abs,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(|e| AppError::Config(format!("upstream_state '{}': {}", us.binding, e)))?;

    // Walk the upstream's own `upstream_state` chain so cycles longer
    // than one hop still surface — but do NOT fetch their state from
    // S3 (or any other backend). The downstream only needs this
    // upstream's own exports; pulling B's transitive upstreams forces
    // A's runtime credentials to cover state buckets B happens to use
    // internally, puncturing the encapsulation that `exports.crn` is
    // supposed to provide (carina-rs/carina#2608).
    Box::pin(walk_upstream_cycles_only(
        &loaded.parsed.upstream_states,
        source_abs,
        provider_context,
        cycle_guard,
    ))
    .await?;

    // Anchor local-backend state paths at the upstream's source directory
    // so `path = "foo.json"` resolves relative to the upstream, not the
    // downstream process's CWD.
    let upstream_backend_config = loaded.parsed.backend.as_ref().map(StateBackendConfig::from);
    let backend: Box<dyn StateBackend> =
        resolve_backend_anchored(upstream_backend_config.as_ref(), source_abs)
            .await
            .map_err(AppError::Backend)?;

    Ok(backend)
}

/// Walk an upstream's transitive `upstream_state` chain *without*
/// performing any state I/O. Detects cycles (as `load_upstream_states`
/// did) but never fetches a state file from a backend — the downstream
/// only needs this upstream's own `exports`, which arrives through the
/// upstream's *own* state file fetched directly by
/// [`load_upstream_states`]. See carina-rs/carina#2608 for the
/// authorization-leak problem the I/O-free walk fixes.
///
/// Returns `Ok(())` on success; the caller does not consume any
/// bindings — there are none.
async fn walk_upstream_cycles_only(
    upstream_states: &[UpstreamState],
    base_dir: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize().map_err(|e| {
            AppError::Config(format!(
                "upstream_state '{}': cannot resolve source '{}': {}",
                us.binding,
                us.source.display(),
                e
            ))
        })?;

        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state '{}': cycle detected at {}",
                us.binding,
                source_abs.display()
            )));
        }

        // Parse the upstream so we can see its own `upstream_state`
        // blocks. We deliberately do NOT touch its backend.
        let loaded = load_configuration_with_config(
            &source_abs,
            provider_context,
            &carina_core::schema::SchemaRegistry::new(),
        )
        .map_err(|e| AppError::Config(format!("upstream_state '{}': {}", us.binding, e)));

        let result = match loaded {
            Ok(loaded) => {
                Box::pin(walk_upstream_cycles_only(
                    &loaded.parsed.upstream_states,
                    &source_abs,
                    provider_context,
                    cycle_guard,
                ))
                .await
            }
            Err(e) => Err(e),
        };

        cycle_guard.remove(&source_abs);
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod load_upstream_states_tests {
    use super::*;
    use std::fs;

    fn write_state(dir: &Path, exports: &[(&str, serde_json::Value)]) {
        let mut state = StateFile::new();
        for (k, v) in exports {
            state.exports.insert(k.to_string(), v.clone());
        }
        fs::write(
            dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn load_upstream_states_reads_exports_from_source_backend() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        write_state(dir.path(), &[("account_id", serde_json::json!("123"))]);

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];

        let base_dir = dir.path().parent().unwrap();
        let result = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap();

        assert_eq!(
            result["orgs"]["account_id"],
            Value::Concrete(ConcreteValue::String("123".to_string()))
        );
    }

    #[tokio::test]
    async fn load_upstream_states_errors_on_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(
            dir_a.join("main.crn"),
            r#"let b = upstream_state { source = "../b" }"#,
        )
        .unwrap();
        fs::write(
            dir_b.join("main.crn"),
            r#"let a = upstream_state { source = "../a" }"#,
        )
        .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "b".to_string(),
            source: PathBuf::from("../b"),
        }];

        let mut guard = HashSet::new();
        guard.insert(dir_a.canonicalize().unwrap());

        let err = load_upstream_states(
            &upstream_states,
            &dir_a,
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {}", err);
    }

    #[tokio::test]
    async fn load_upstream_states_errors_when_source_missing() {
        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: PathBuf::from("/nonexistent/carina/upstream/path"),
        }];
        let err = load_upstream_states(
            &upstream_states,
            Path::new("/"),
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("orgs"),
            "error should name the binding: {}",
            err
        );
    }

    /// Strict policy with a parseable upstream that has *no* `carina.state.json`
    /// must error with `"no state found at <path>"` — this is the contract
    /// `apply` relies on (#2366).
    #[tokio::test]
    async fn load_upstream_states_strict_errors_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        // Intentionally do NOT write carina.state.json.

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let base_dir = dir.path().parent().unwrap();
        let err = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no state found at"),
            "expected `no state found at`, got: {}",
            msg
        );
        assert!(
            msg.contains("orgs"),
            "error should name the binding: {}",
            msg
        );
    }

    /// Lenient policy with the same setup must NOT error — it warns and
    /// returns an empty binding so downstream display can stamp the marker.
    #[tokio::test]
    async fn load_upstream_states_lenient_returns_empty_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let base_dir = dir.path().parent().unwrap();
        let result = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Lenient,
        )
        .await
        .expect("lenient must not error");
        assert!(
            result.contains_key("orgs"),
            "binding name must be present so display can stamp the marker"
        );
        assert!(
            result["orgs"].is_empty(),
            "no exports available, so the inner map must be empty"
        );
    }

    /// carina-rs/carina#2608: when component A reads B's
    /// `upstream_state`, carina must NOT walk into B's own
    /// transitive upstreams' state files. If it does, A's runtime
    /// credentials need read access to state objects A never
    /// references — puncturing the encapsulation that
    /// `exports.crn` is supposed to provide.
    ///
    /// The fixture: A reads B; B internally reads C. A's
    /// `load_upstream_states` should fetch B's state file (which
    /// has its `exports` already serialized), but must NEVER touch
    /// C's state file. We assert the latter by deliberately giving
    /// C a state file at a path that would parse but with sentinel
    /// `exports`, then by *not* asserting anything about C — and by
    /// covering the related path (cycle detection still walks the
    /// chain) with the existing `cycle_detected_*` tests.
    ///
    /// Direct repro of the authorization-leak shape: we delete C's
    /// state file. A pre-fix run loads C and crashes with
    /// `no state found at <C>` under Strict policy; a post-fix
    /// run does not visit C at all.
    #[tokio::test]
    async fn load_upstream_states_does_not_fetch_transitive_upstream_state() {
        let root = tempfile::tempdir().unwrap();
        let a_dir = root.path().join("a");
        let b_dir = root.path().join("b");
        let c_dir = root.path().join("c");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();
        fs::create_dir_all(&c_dir).unwrap();

        // C declares a backend; we deliberately do NOT write a
        // state file. Loading C's state would fail with
        // `BackendError` under Strict policy — the post-fix path
        // must skip this entirely.
        fs::write(
            c_dir.join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();

        // B reads C as an upstream and has its own backend with
        // exports serialized into its state file.
        fs::write(
            b_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let internal = upstream_state {{ source = '{}' }}"#,
                c_dir.display()
            ),
        )
        .unwrap();
        write_state(
            &b_dir,
            &[("oidc_provider_arn", serde_json::json!("arn:..."))],
        );

        // A reads B.
        let upstream_states = vec![UpstreamState {
            binding: "bootstrap".to_string(),
            source: b_dir.clone(),
        }];

        let result = load_upstream_states(
            &upstream_states,
            &a_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await;

        // The successful return is the assertion: C had no state
        // file, but the loader must not have tried to read it. If
        // the pre-fix transitive-fetch behavior were still active,
        // this would fail with "no state found at <c>".
        let bindings = result.expect(
            "loader must succeed without visiting C; carina-rs/carina#2608 \
             regression if this becomes `no state found at <c>`",
        );
        assert_eq!(
            bindings["bootstrap"]["oidc_provider_arn"],
            Value::Concrete(ConcreteValue::String("arn:...".to_string())),
            "B's exports must come through unchanged"
        );
    }

    /// Regression guard: cycles longer than one hop are still
    /// detected even though we no longer fetch transitive state.
    /// A → B → A must error, not stack-overflow or deadlock.
    #[tokio::test]
    async fn load_upstream_states_detects_two_hop_cycle_without_fetching_state() {
        let root = tempfile::tempdir().unwrap();
        let a_dir = root.path().join("a");
        let b_dir = root.path().join("b");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();

        // A declares an upstream pointing at B; B declares an
        // upstream pointing back at A — a cycle.
        fs::write(
            a_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let back = upstream_state {{ source = '{}' }}"#,
                b_dir.display()
            ),
        )
        .unwrap();
        fs::write(
            b_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let back = upstream_state {{ source = '{}' }}"#,
                a_dir.display()
            ),
        )
        .unwrap();

        // Pretend a "downstream" reads A; the loader must catch the
        // A → B → A cycle.
        let upstream_states = vec![UpstreamState {
            binding: "a".to_string(),
            source: a_dir.clone(),
        }];

        let mut guard = HashSet::new();
        // Seed the guard so A → B → A detection still triggers
        // when the downstream itself is the seed; mirrors how the
        // CLI seeds with the project root before recursing.
        guard.insert(root.path().canonicalize().unwrap());

        let err = load_upstream_states(
            &upstream_states,
            root.path(),
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .expect_err("two-hop cycle must error");
        let msg = err.to_string();
        assert!(
            msg.contains("cycle detected"),
            "cycle detection must still fire post-#2608 fix, got: {msg}"
        );
    }
}

#[cfg(test)]
mod run_plan_upstream_state_tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn run_plan_resolves_upstream_state_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        fs::write(
            dir_a.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
exports { region: String = "ap-northeast-1" }"#,
        )
        .unwrap();

        let mut state_a = StateFile::new();
        state_a
            .exports
            .insert("region".to_string(), serde_json::json!("ap-northeast-1"));
        fs::write(
            dir_a.join("carina.state.json"),
            serde_json::to_string(&state_a).unwrap(),
        )
        .unwrap();

        fs::write(
            dir_b.join("main.crn"),
            r#"
                let a = upstream_state { source = "../a" }
                exports { region = a.region }
            "#,
        )
        .unwrap();

        crate::commands::ensure_backend_lock(&dir_b, None).unwrap();

        run_plan(
            &dir_b,
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("run_plan should succeed");

        // `run_plan` returns only `has_changes`; reload the downstream via
        // the same loader path to verify the upstream binding value is
        // reachable to downstream references.
        let parsed = carina_core::config_loader::load_configuration_with_config(
            &dir_b,
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load config")
        .parsed;
        let mut guard = seed_cycle_guard(&dir_b);
        let bindings = load_upstream_states(
            &parsed.upstream_states,
            &dir_b,
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .expect("upstream bindings");
        assert_eq!(
            bindings["a"]["region"],
            Value::Concrete(ConcreteValue::String("ap-northeast-1".to_string()))
        );
    }
}

#[cfg(test)]
mod run_plan_out_tests {
    use super::*;
    use std::fs;

    // Pin the byte-level shape so plan files written by `plan --out`
    // match the trailing-newline convention used by carina-backend.lock,
    // carina.state.json, and carina-providers.lock (#2583).
    #[tokio::test]
    async fn run_plan_out_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("main.crn"),
            r#"exports { region: String = "ap-northeast-1" }"#,
        )
        .unwrap();

        crate::commands::ensure_backend_lock(dir, None).unwrap();

        let plan_path = dir.join("plan.json");
        run_plan(
            dir,
            Some(&plan_path),
            DetailLevel::None,
            false,
            false,
            true,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("run_plan should succeed");

        let bytes = fs::read(&plan_path).expect("plan file written");
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "plan --out file must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }
}

#[cfg(test)]
mod export_diff_tests {
    use super::*;
    use carina_core::parser::{InferredExportParam, TypeExpr};

    fn param(name: &str, value: Value) -> InferredExportParam {
        InferredExportParam {
            name: name.to_string(),
            type_expr: TypeExpr::Unknown,
            value: Some(value),
        }
    }

    #[test]
    fn compute_export_diffs_added_when_state_empty() {
        let params = vec![param("count", Value::Concrete(ConcreteValue::Int(42)))];
        let current = HashMap::new();
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Added { .. }));
    }

    #[test]
    fn compute_export_diffs_modified_when_value_differs() {
        let params = vec![param("count", Value::Concrete(ConcreteValue::Int(42)))];
        let mut current = HashMap::new();
        current.insert("count".to_string(), serde_json::json!(7));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Modified { .. }));
    }

    #[test]
    fn compute_export_diffs_unchanged_when_value_matches() {
        let params = vec![param("count", Value::Concrete(ConcreteValue::Int(42)))];
        let mut current = HashMap::new();
        current.insert("count".to_string(), serde_json::json!(42));
        let changes = compute_export_diffs(&params, &current);
        assert!(changes.is_empty());
    }

    #[test]
    fn compute_export_diffs_removed_when_param_missing() {
        let params = vec![];
        let mut current = HashMap::new();
        current.insert("stale".to_string(), serde_json::json!("old"));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Removed { .. }));
    }

    #[test]
    fn compute_export_diffs_mixed_sorted_by_name() {
        let params = vec![
            param("added", Value::Concrete(ConcreteValue::Int(1))),
            param("modified", Value::Concrete(ConcreteValue::Int(2))),
        ];
        let mut current = HashMap::new();
        current.insert("modified".to_string(), serde_json::json!(99));
        current.insert("removed".to_string(), serde_json::json!("old"));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].name(), "added");
        assert_eq!(changes[1].name(), "modified");
        assert_eq!(changes[2].name(), "removed");
    }
}

#[cfg(test)]
mod plan_serialization_error_tests {
    //! RFC #2371 stage 4: serialization-boundary errors are now produced
    //! by `value_to_json` / `redact_secrets_in_*` directly, surfaced
    //! through `build_plan_file`'s `SerializationError` return. Verify
    //! the actionable wording and that every effect shape (top-level
    //! attributes, nested List/Map, Replace cascade) propagates the
    //! error.
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, ManagedResource, UnknownReason, Value,
    };
    use carina_core::value::{SerializationError, redact_secrets_in_resource, value_to_json};

    fn upstream_unknown() -> Value {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef { path }))
    }

    #[test]
    fn value_to_json_errors_on_top_level_unknown() {
        let err = value_to_json(&upstream_unknown()).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamRef { path },
                ..
            } => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected UnknownNotAllowed/UpstreamRef, got: {other:?}"),
        }
    }

    #[test]
    fn value_to_json_errors_on_unknown_inside_list_or_map() {
        let list_val = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            upstream_unknown(),
        ]));
        assert!(matches!(
            value_to_json(&list_val).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));

        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("Name".into(), upstream_unknown());
        assert!(matches!(
            value_to_json(&Value::Concrete(ConcreteValue::Map(m))).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));
    }

    #[test]
    fn redact_secrets_in_resource_errors_on_unknown_attribute() {
        let mut r = ManagedResource::new("test.resource", "name");
        r.attributes.insert("vpc_id".into(), upstream_unknown());
        let err = redact_secrets_in_resource(&r).unwrap_err();
        assert!(matches!(err, SerializationError::UnknownNotAllowed { .. }));
    }

    /// `for*` placeholders carry no path, but their reason variant
    /// must survive into the error so the caller can render
    /// "for-binding placeholder" rather than a generic message.
    #[test]
    fn value_to_json_preserves_for_variants_in_error() {
        for (variant, label) in [
            (UnknownReason::ForValue, "ForValue"),
            (UnknownReason::ForKey, "ForKey"),
            (UnknownReason::ForIndex, "ForIndex"),
        ] {
            let err = value_to_json(&Value::Deferred(DeferredValue::Unknown(variant.clone())))
                .unwrap_err();
            match err {
                SerializationError::UnknownNotAllowed { reason, .. } => {
                    assert_eq!(reason, variant, "expected {label} round-trip");
                }
                other => panic!("expected UnknownNotAllowed for {label}, got: {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod state_drift_tests {
    use super::*;

    fn state(serial: u64, lineage: &str) -> StateFile {
        let mut s = StateFile::new();
        s.serial = serial;
        s.lineage = lineage.to_string();
        s
    }

    #[test]
    fn no_state_at_t0_means_no_drift() {
        let before = StateSnapshot::capture(None);
        let after = state(5, "abc");
        assert_eq!(detect_state_drift(before.as_ref(), Some(&after)), None);
    }

    #[test]
    fn unchanged_serial_and_lineage_is_no_drift() {
        let s = state(7, "abc");
        let before = StateSnapshot::capture(Some(&s));
        assert_eq!(detect_state_drift(before.as_ref(), Some(&s)), None);
    }

    #[test]
    fn advanced_serial_same_lineage_is_serial_drift() {
        let t0 = state(7, "abc");
        let before = StateSnapshot::capture(Some(&t0));
        let t1 = state(8, "abc");
        assert_eq!(
            detect_state_drift(before.as_ref(), Some(&t1)),
            Some(StateDrift::SerialAdvanced { from: 7, to: 8 })
        );
    }

    #[test]
    fn changed_lineage_is_lineage_drift_even_if_serial_lower() {
        let t0 = state(9, "old-lineage");
        let before = StateSnapshot::capture(Some(&t0));
        // Fresh state after destroy+apply: new lineage, serial reset.
        let t1 = state(0, "new-lineage");
        assert_eq!(
            detect_state_drift(before.as_ref(), Some(&t1)),
            Some(StateDrift::LineageChanged {
                from: "old-lineage".to_string(),
                to: "new-lineage".to_string(),
            })
        );
    }

    #[test]
    fn state_removed_between_t0_and_t1_is_removed_drift() {
        let t0 = state(3, "abc");
        let before = StateSnapshot::capture(Some(&t0));
        assert_eq!(
            detect_state_drift(before.as_ref(), None),
            Some(StateDrift::StateRemoved)
        );
    }

    #[test]
    fn warning_message_names_the_drift_kind() {
        assert!(
            StateDrift::SerialAdvanced { from: 1, to: 2 }
                .warning()
                .contains("serial advanced 1 -> 2")
        );
        assert!(
            StateDrift::LineageChanged {
                from: "a".into(),
                to: "b".into()
            }
            .warning()
            .contains("lineage changed a -> b")
        );
        assert!(
            StateDrift::StateRemoved
                .warning()
                .contains("state was removed")
        );
        // Every variant must steer the user to re-run plan.
        assert!(
            StateDrift::StateRemoved
                .warning()
                .contains("re-run `carina plan`")
        );
    }
}
