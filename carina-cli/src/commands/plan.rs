use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::effect::Effect;
use carina_core::parser::{BackendConfig, ProviderConfig, ProviderContext, RemoteState};
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
    WiringContext, create_plan_from_parsed_with_remote, reconcile_anonymous_identifiers_with_ctx,
    reconcile_prefixed_names,
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

pub async fn run_plan(
    path: &PathBuf,
    out: Option<&PathBuf>,
    detail: DetailLevel,
    tui: bool,
    refresh: bool,
    provider_context: &ProviderContext,
) -> Result<bool, AppError> {
    let mut parsed = load_configuration_with_config(path, provider_context)?.parsed;

    let base_dir = get_base_dir(path);
    validate_and_resolve_with_config(&mut parsed, base_dir, false, provider_context)?;

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

    let wiring = WiringContext::new();
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

    // Load remote state data sources
    let remote_bindings = load_remote_states(&parsed.remote_states, base_dir)?;

    let ctx = create_plan_from_parsed_with_remote(&parsed, &state_file, refresh, &remote_bindings)
        .await?;
    let has_changes = ctx.plan.mutation_count() > 0;

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

    if tui {
        carina_tui::run(&ctx.plan, wiring.schemas())
            .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
    } else {
        print_plan(
            &ctx.plan,
            detail,
            &delete_attributes,
            Some(wiring.schemas()),
            &ctx.moved_origins,
        );
    }

    // Save plan to file if --out was specified
    if let Some(out_path) = out {
        // Redact secrets before serializing to prevent plaintext secret leakage
        let plan_file = PlanFile {
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
        };

        let json = serde_json::to_string_pretty(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        fs::write(out_path, json).map_err(|e| format!("Failed to write plan file: {}", e))?;

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
fn load_remote_states(
    remote_states: &[RemoteState],
    base_dir: &Path,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut result = HashMap::new();

    for rs in remote_states {
        let state_path = if Path::new(&rs.path).is_absolute() {
            PathBuf::from(&rs.path)
        } else {
            base_dir.join(&rs.path)
        };

        let content = fs::read_to_string(&state_path).map_err(|e| {
            AppError::Config(format!(
                "Failed to read remote state file '{}' for remote_state '{}': {}",
                state_path.display(),
                rs.binding,
                e
            ))
        })?;

        let state_file = check_and_migrate(&content).map_err(|e| {
            AppError::Config(format!(
                "Failed to parse remote state file '{}' for remote_state '{}': {}",
                state_path.display(),
                rs.binding,
                e
            ))
        })?;

        let bindings = state_file.build_remote_bindings();
        result.insert(rs.binding.clone(), bindings);
    }

    Ok(result)
}
