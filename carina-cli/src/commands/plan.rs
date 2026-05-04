use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::effect::Effect;
use carina_core::parser::{BackendConfig, ProviderConfig, ProviderContext, UpstreamState};
use carina_core::plan::Plan;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::value::{
    redact_secrets_in_plan, redact_secrets_in_resource, redact_secrets_in_state,
};
use carina_state::{
    BackendConfig as StateBackendConfig, LocalBackend, StateBackend, StateFile, create_backend,
    create_local_backend, resolve_backend,
};

use super::validate_and_resolve_with_config;
use crate::DetailLevel;
use crate::commands::shared::state_writeback::apply_name_overrides;
use crate::display::print_plan;
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
    pub sorted_resources: Vec<Resource>,
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
        version: 2,
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

#[allow(clippy::too_many_arguments)]
pub async fn run_plan(
    path: &PathBuf,
    out: Option<&PathBuf>,
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
                    Value::String(s) => Some(s.clone()),
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
                        .is_some_and(|v| matches!(v, Value::String(s) if s == &bucket_name))
            });

            if !has_bucket_resource {
                let auto_create = config
                    .attributes
                    .get("auto_create")
                    .and_then(|v| match v {
                        Value::Bool(b) => Some(*b),
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
            "  {} {} (state bucket with versioning enabled)",
            "+".green(),
            format!(
                "{}.{}.{}",
                backend_provider, backend_resource_type, state_bucket_name
            )
            .green()
        );
        println!(
            "  {} Resource definition will be added to .crn file",
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

    // Expand deferred for-expressions now that remote values are available
    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Print warnings after expansion (resolved deferred for-expressions have their warnings removed)
    parsed.print_warnings();

    let ctx = create_plan_from_parsed_with_upstream(
        &parsed,
        &state_file,
        refresh,
        &remote_bindings,
        base_dir,
    )
    .await?;
    let has_changes = ctx.plan.mutation_count() > 0;

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
        let resolved_exports = resolve_export_values_for_display(
            &parsed.export_params,
            &ctx.sorted_resources,
            &ctx.current_states,
        );
        let current_exports = state_file
            .as_ref()
            .map(|s| s.exports.clone())
            .unwrap_or_default();
        let export_changes = compute_export_diffs(&resolved_exports, &current_exports);
        print_plan(
            &ctx.plan,
            detail,
            &delete_attributes,
            Some(wiring.schemas()),
            &ctx.moved_origins,
            &export_changes,
            &parsed.deferred_for_expressions,
        );
    }

    // Save plan to file if --out was specified
    if let Some(out_path) = out {
        let plan_file = build_plan_file(path, &parsed, &state_file, &ctx)
            .map_err(|e| format_plan_save_error(&e, "--out"))?;
        let json_out = serde_json::to_string_pretty(&plan_file)
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
/// and values are `Value::Map` of that resource's attributes.
/// Resolve export value expressions for plan display.
pub(crate) fn resolve_export_values_for_display(
    export_params: &[carina_core::parser::InferredExportParam],
    resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
) -> Vec<carina_core::parser::InferredExportParam> {
    let bindings = carina_core::binding_index::ResolvedBindings::from_resources_with_state(
        resources,
        current_states,
        &HashMap::new(),
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
        Value::ResourceRef { .. } => {
            resolve_ref_value(value, bindings).unwrap_or_else(|_| value.clone())
        }
        // Cross-file: "binding.attr" parsed as String instead of ResourceRef
        Value::String(s) if s.contains('.') && !s.contains(' ') => {
            let parts: Vec<&str> = s.splitn(2, '.').collect();
            if parts.len() == 2
                && let Some(attrs) = bindings.get(parts[0])
                && let Some(resolved) = attrs.get(parts[1])
            {
                return resolved.clone();
            }
            value.clone()
        }
        Value::List(items) => {
            let resolved: Vec<Value> = items
                .iter()
                .map(|item| resolve_export_value(item, bindings))
                .collect();
            Value::List(resolved)
        }
        Value::Map(map) => {
            let resolved: indexmap::IndexMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), resolve_export_value(v, bindings)))
                .collect();
            Value::Map(resolved)
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
        // Plan display tolerates `Value::Unknown` in the export rhs
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
            build_upstream_backend(us, &source_abs, provider_context, cycle_guard, policy).await;
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
/// chain (cycle detection), then build the backend honoring local-path
/// anchoring. Shared between strict and lenient upstream loaders so the
/// only behavioral difference is how a `None` from `read_state()` is
/// handled.
async fn build_upstream_backend(
    us: &UpstreamState,
    source_abs: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
    policy: UpstreamMissingStatePolicy,
) -> Result<Box<dyn StateBackend>, AppError> {
    let loaded = load_configuration_with_config(
        &source_abs.to_path_buf(),
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(|e| AppError::Config(format!("upstream_state '{}': {}", us.binding, e)))?;

    // Walk the upstream's own upstream_state blocks so cycles are detected
    // even when the chain is longer than one hop. The returned bindings are
    // discarded; the downstream only needs this upstream's own exports.
    // Propagate `policy` so a chained upstream that is itself unapplied
    // produces a warning under Lenient instead of breaking the plan (#2366).
    Box::pin(load_upstream_states(
        &loaded.parsed.upstream_states,
        source_abs,
        provider_context,
        cycle_guard,
        policy,
    ))
    .await?;

    let backend: Box<dyn StateBackend> = match loaded.parsed.backend.as_ref() {
        // Anchor local-backend state paths at the upstream's source directory
        // so `path = "foo.json"` resolves relative to the upstream, not the
        // downstream process's CWD.
        Some(config) if config.backend_type == "local" => {
            let state_path = StateBackendConfig::from(config)
                .get_string("path")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(LocalBackend::DEFAULT_STATE_FILE));
            let anchored = if state_path.is_absolute() {
                state_path
            } else {
                source_abs.join(state_path)
            };
            Box::new(LocalBackend::with_path(anchored))
        }
        Some(config) => resolve_backend(Some(config))
            .await
            .map_err(AppError::Backend)?,
        None => Box::new(LocalBackend::with_path(
            source_abs.join(LocalBackend::DEFAULT_STATE_FILE),
        )),
    };

    Ok(backend)
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
            Value::String("123".to_string())
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
            Value::String("ap-northeast-1".to_string())
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
        let params = vec![param("count", Value::Int(42))];
        let current = HashMap::new();
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Added { .. }));
    }

    #[test]
    fn compute_export_diffs_modified_when_value_differs() {
        let params = vec![param("count", Value::Int(42))];
        let mut current = HashMap::new();
        current.insert("count".to_string(), serde_json::json!(7));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Modified { .. }));
    }

    #[test]
    fn compute_export_diffs_unchanged_when_value_matches() {
        let params = vec![param("count", Value::Int(42))];
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
            param("added", Value::Int(1)),
            param("modified", Value::Int(2)),
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
    use carina_core::resource::{AccessPath, Resource, UnknownReason, Value};
    use carina_core::value::{SerializationError, redact_secrets_in_resource, value_to_json};

    fn upstream_unknown() -> Value {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        Value::Unknown(UnknownReason::UpstreamRef { path })
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
        let list_val = Value::List(vec![Value::String("a".into()), upstream_unknown()]);
        assert!(matches!(
            value_to_json(&list_val).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));

        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("Name".into(), upstream_unknown());
        assert!(matches!(
            value_to_json(&Value::Map(m)).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));
    }

    #[test]
    fn redact_secrets_in_resource_errors_on_unknown_attribute() {
        let mut r = Resource::new("test.resource", "name");
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
            let err = value_to_json(&Value::Unknown(variant.clone())).unwrap_err();
            match err {
                SerializationError::UnknownNotAllowed { reason, .. } => {
                    assert_eq!(reason, variant, "expected {label} round-trip");
                }
                other => panic!("expected UnknownNotAllowed for {label}, got: {other:?}"),
            }
        }
    }
}
