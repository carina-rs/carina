use std::collections::HashMap;
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
    BackendConfig as StateBackendConfig, StateBackend, StateFile, check_and_migrate,
    create_backend, create_local_backend,
};

use super::validate_and_resolve_with_config;
use crate::DetailLevel;
use crate::commands::apply::apply_name_overrides;
use crate::display::print_plan;
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, create_plan_from_parsed_with_remote,
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
}

/// Entry for serializing current resource states
#[derive(Debug, Serialize, Deserialize)]
pub struct CurrentStateEntry {
    pub id: ResourceId,
    pub state: State,
}

fn build_plan_file(
    path: &Path,
    parsed: &carina_core::parser::ParsedFile,
    state_file: &Option<StateFile>,
    ctx: &crate::wiring::PlanContext,
) -> PlanFile {
    PlanFile {
        version: 1,
        carina_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        source_path: path.display().to_string(),
        state_lineage: state_file.as_ref().map(|s| s.lineage.clone()),
        state_serial: state_file.as_ref().map(|s| s.serial),
        provider_configs: parsed.providers.clone(),
        backend_config: parsed.backend.clone(),
        plan: redact_secrets_in_plan(&ctx.plan),
        sorted_resources: ctx
            .sorted_resources
            .iter()
            .map(redact_secrets_in_resource)
            .collect(),
        current_states: ctx
            .current_states
            .iter()
            .map(|(id, state)| CurrentStateEntry {
                id: id.clone(),
                state: redact_secrets_in_state(state),
            })
            .collect(),
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
    let mut parsed = load_configuration_with_config(path, provider_context)?.parsed;

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
            let has_bucket_resource = parsed.resources.iter().any(|r| {
                r.id.resource_type == backend_resource_type
                    && r.attributes
                        .get("bucket")
                        .is_some_and(|v| matches!(&**v, Value::String(s) if s == &bucket_name))
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
        reconcile_anonymous_identifiers_with_ctx(&wiring, &mut parsed.resources, sf);
    }
    apply_name_overrides(&mut parsed.resources, &state_file);

    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached state (--refresh=false). Plan may not reflect actual infrastructure.".yellow()
        );
    }

    let remote_bindings = load_upstream_state_bindings(&parsed.upstream_states, base_dir).await?;

    // Expand deferred for-expressions now that remote values are available
    parsed.expand_deferred_for_expressions(&remote_bindings);

    // Print warnings after expansion (resolved deferred for-expressions have their warnings removed)
    parsed.print_warnings();

    let ctx = create_plan_from_parsed_with_remote(
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
        let plan_file = build_plan_file(path, &parsed, &state_file, &ctx);
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
        let plan_file = build_plan_file(path, &parsed, &state_file, &ctx);
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
    export_params: &[carina_core::parser::ExportParameter],
    resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
) -> Vec<carina_core::parser::ExportParameter> {
    // Build binding map from resources + current state
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            let mut attrs: HashMap<String, Value> =
                carina_core::resource::Expr::resolve_map(&resource.attributes);
            if let Some(state) = current_states.get(&resource.id)
                && state.exists
            {
                for (k, v) in &state.attributes {
                    if !attrs.contains_key(k) {
                        attrs.insert(k.clone(), v.clone());
                    }
                }
            }
            binding_map.insert(binding_name.clone(), attrs);
        }
    }

    export_params
        .iter()
        .map(|param| {
            let resolved_value = param
                .value
                .as_ref()
                .map(|v| resolve_export_value(v, &binding_map));
            carina_core::parser::ExportParameter {
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
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Value {
    use carina_core::resolver::resolve_ref_value;

    match value {
        Value::ResourceRef { .. } => {
            resolve_ref_value(value, binding_map).unwrap_or_else(|_| value.clone())
        }
        // Cross-file: "binding.attr" parsed as String instead of ResourceRef
        Value::String(s) if s.contains('.') && !s.contains(' ') => {
            let parts: Vec<&str> = s.splitn(2, '.').collect();
            if parts.len() == 2
                && let Some(attrs) = binding_map.get(parts[0])
                && let Some(resolved) = attrs.get(parts[1])
            {
                return resolved.clone();
            }
            value.clone()
        }
        Value::List(items) => {
            let resolved: Vec<Value> = items
                .iter()
                .map(|item| resolve_export_value(item, binding_map))
                .collect();
            Value::List(resolved)
        }
        Value::Map(map) => {
            let resolved: HashMap<String, Value> = map
                .iter()
                .map(|(k, v)| (k.clone(), resolve_export_value(v, binding_map)))
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
    resolved_params: &[carina_core::parser::ExportParameter],
    current_exports: &HashMap<String, serde_json::Value>,
) -> Vec<ExportChange> {
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for param in resolved_params {
        seen.insert(param.name.clone());
        let Some(ref value) = param.value else {
            continue;
        };
        let new_json = crate::commands::apply::dsl_value_to_json(value);
        match (current_exports.get(&param.name), new_json) {
            (None, _) => changes.push(ExportChange::Added {
                name: param.name.clone(),
                type_expr: param.type_expr.clone(),
                new_value: value.clone(),
            }),
            (Some(old), Some(new)) if old == &new => {
                // unchanged — skip
            }
            (Some(old), _) => changes.push(ExportChange::Modified {
                name: param.name.clone(),
                type_expr: param.type_expr.clone(),
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

/// Resolve the state-file path for an `upstream_state` block. The `source`
/// is treated as a directory (absolute or relative to `base_dir`) whose
/// default local state file is read.
pub(crate) fn upstream_state_file_path(us: &UpstreamState, base_dir: &Path) -> PathBuf {
    let dir = if us.source.is_absolute() {
        us.source.clone()
    } else {
        base_dir.join(&us.source)
    };
    dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE)
}

pub(crate) async fn load_upstream_state_bindings(
    upstream_states: &[UpstreamState],
    base_dir: &Path,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut result = HashMap::new();

    for us in upstream_states {
        let state_path = upstream_state_file_path(us, base_dir);

        let content = fs::read_to_string(&state_path).map_err(|e| {
            AppError::Config(format!(
                "Failed to read upstream state file '{}' for upstream_state '{}': {}",
                state_path.display(),
                us.binding,
                e
            ))
        })?;

        let state_file = check_and_migrate(&content).map_err(|e| {
            AppError::Config(format!(
                "Failed to parse upstream state file '{}' for upstream_state '{}': {}",
                state_path.display(),
                us.binding,
                e
            ))
        })?;

        result.insert(us.binding.clone(), state_file.build_remote_bindings());
    }

    Ok(result)
}

#[cfg(test)]
mod export_diff_tests {
    use super::*;
    use carina_core::parser::ExportParameter;

    fn param(name: &str, value: Value) -> ExportParameter {
        ExportParameter {
            name: name.to_string(),
            type_expr: None,
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
