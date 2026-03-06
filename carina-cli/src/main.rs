mod display;
mod wiring;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use carina_core::config_loader::{
    find_crn_files_in_dir, find_crn_files_recursive, get_base_dir, load_configuration,
};
use carina_core::deps::{
    build_dependents_map, find_failed_dependency, find_failed_dependent,
    sort_resources_by_dependencies,
};
use carina_core::differ::create_plan;
use carina_core::effect::Effect;
use carina_core::formatter::{self, FormatConfig};
use carina_core::lint::{find_list_literal_attrs, list_struct_attr_names};
use carina_core::module_resolver;
#[cfg(test)]
use carina_core::parser::ParsedFile;
use carina_core::parser::{BackendConfig, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider};
use carina_core::resolver::{resolve_ref_value, resolve_refs_with_state};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::value::{format_value, json_to_dsl_value};
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, LockInfo, ResourceState, StateBackend,
    StateFile, create_backend, create_local_backend,
};
use std::collections::HashSet;

use display::{format_effect, print_plan};
#[cfg(test)]
use wiring::resolve_attr_prefixes;
use wiring::{
    compute_anonymous_identifiers, create_plan_from_parsed, create_provider_from_config,
    get_provider, get_schemas, provider_factories, reconcile_prefixed_names, resolve_names,
    validate_module_calls, validate_provider_region, validate_resource_ref_types,
    validate_resources,
};

#[derive(Parser)]
#[command(name = "carina")]
#[command(about = "A functional infrastructure management tool", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate the configuration file
    Validate {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Show execution plan without applying changes
    Plan {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Save plan to a file for later apply
        #[arg(long = "out")]
        out: Option<PathBuf>,

        /// Return exit code 2 when changes are present
        #[arg(long = "detailed-exitcode")]
        detailed_exitcode: bool,
    },
    /// Apply changes to reach the desired state
    Apply {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,
    },
    /// Destroy all resources defined in the configuration file
    Destroy {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Skip confirmation prompt (auto-approve)
        #[arg(long)]
        auto_approve: bool,
    },
    /// Format .crn files
    Fmt {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,

        /// Check if files are formatted (don't modify)
        #[arg(long, short)]
        check: bool,

        /// Show diff of formatting changes
        #[arg(long)]
        diff: bool,

        /// Recursively format all .crn files in directory
        #[arg(long, short)]
        recursive: bool,
    },
    /// Module management commands
    Module {
        #[command(subcommand)]
        command: ModuleCommands,
    },
    /// Force unlock a stuck state lock
    ForceUnlock {
        /// The lock ID to force unlock
        lock_id: String,

        /// Path to .crn file or directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// State management commands
    State {
        #[command(subcommand)]
        command: StateCommands,
    },
    /// Lint .crn files for style issues
    Lint {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Show module structure and dependencies
    Info {
        /// Path to module .crn file
        file: PathBuf,
    },
}

#[derive(Subcommand)]
enum StateCommands {
    /// Delete state bucket (requires --force flag)
    BucketDelete {
        /// Name of the bucket to delete
        bucket_name: String,

        /// Force deletion without confirmation
        #[arg(long)]
        force: bool,

        /// Path to .crn file or directory containing backend configuration
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Refresh state from cloud providers without planning or applying
    Refresh {
        /// Path to .crn file or directory
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

/// Saved plan file for `plan --out` / `apply plan.json`
#[derive(Debug, Serialize, Deserialize)]
struct PlanFile {
    /// Plan file format version
    version: u32,
    /// Carina version that created this plan
    carina_version: String,
    /// ISO 8601 timestamp
    timestamp: String,
    /// Original .crn path (informational)
    source_path: String,
    /// State lineage for drift detection
    state_lineage: Option<String>,
    /// State serial for drift detection
    state_serial: Option<u64>,
    /// Provider configuration
    provider_config: ProviderConfig,
    /// Backend configuration
    backend_config: Option<BackendConfig>,
    /// The plan (effects)
    plan: Plan,
    /// Resources sorted by dependencies (for post-apply state saving)
    sorted_resources: Vec<Resource>,
    /// Current states (for binding_map + state saving)
    current_states: Vec<CurrentStateEntry>,
}

/// Entry for serializing current resource states
#[derive(Debug, Serialize, Deserialize)]
struct CurrentStateEntry {
    id: ResourceId,
    state: State,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Handle Plan separately since it returns Result<bool, String>
    if let Commands::Plan {
        path,
        out,
        detailed_exitcode,
    } = cli.command
    {
        match run_plan(&path, out.as_ref()).await {
            Ok(has_changes) => {
                if detailed_exitcode && has_changes {
                    std::process::exit(2);
                }
            }
            Err(e) => {
                eprintln!("{} {}", "Error:".red().bold(), e);
                std::process::exit(1);
            }
        }
        return;
    }

    let result = match cli.command {
        Commands::Validate { path } => run_validate(&path),
        Commands::Plan { .. } => unreachable!(),
        Commands::Apply { path, auto_approve } => {
            if path.extension().is_some_and(|ext| ext == "json") {
                run_apply_from_plan(&path, auto_approve).await
            } else {
                run_apply(&path, auto_approve).await
            }
        }
        Commands::Destroy { path, auto_approve } => run_destroy(&path, auto_approve).await,
        Commands::Fmt {
            path,
            check,
            diff,
            recursive,
        } => run_fmt(&path, check, diff, recursive),
        Commands::Module { command } => run_module_command(command),
        Commands::ForceUnlock { lock_id, path } => run_force_unlock(&lock_id, &path).await,
        Commands::State { command } => run_state_command(command).await,
        Commands::Lint { path } => run_lint(&path),
        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "carina", &mut std::io::stdout());
            Ok(())
        }
    };

    if let Err(e) = result {
        eprintln!("{} {}", "Error:".red().bold(), e);
        std::process::exit(1);
    }
}

fn run_module_command(command: ModuleCommands) -> Result<(), String> {
    match command {
        ModuleCommands::Info { file } => run_module_info(&file),
    }
}

fn run_module_info(path: &Path) -> Result<(), String> {
    let parsed = if path.is_dir() {
        // Read all .crn files in the directory and merge them
        module_resolver::load_module_from_directory(path)?
    } else {
        module_resolver::get_parsed_file(path).map_err(|e| format!("Failed to load file: {}", e))?
    };

    // Derive module name from directory structure
    // For directory-based modules like modules/web_tier/, use the directory name
    // For file-based modules like modules/web_tier.crn, use the file stem
    let module_name = module_resolver::derive_module_name(path);

    // Build and display the file signature (module or root config)
    let signature =
        carina_core::module::FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    println!("{}", signature.display());

    Ok(())
}

fn run_validate(path: &PathBuf) -> Result<(), String> {
    let mut parsed = load_configuration(path)?.parsed;

    let base_dir = get_base_dir(path);

    // Validate provider region
    validate_provider_region(&parsed)?;

    // Validate module call arguments before expansion
    validate_module_calls(&parsed, base_dir)?;

    // Resolve module imports and expand module calls
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    println!("{}", "Validating...".cyan());

    resolve_names(&mut parsed.resources)?;
    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    println!(
        "{}",
        format!(
            "✓ {} resources validated successfully.",
            parsed.resources.len()
        )
        .green()
        .bold()
    );

    for resource in &parsed.resources {
        println!("  • {}", resource.id);
    }

    Ok(())
}

async fn run_plan(path: &PathBuf, out: Option<&PathBuf>) -> Result<bool, String> {
    let mut parsed = load_configuration(path)?.parsed;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Validate provider region
    validate_provider_region(&parsed)?;

    resolve_names(&mut parsed.resources)?;
    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    // Check for backend configuration and load state
    // Use local backend by default if no backend is configured
    let mut will_create_state_bucket = false;
    let mut state_bucket_name = String::new();
    let mut state_file: Option<StateFile> = None;

    let plan_backend: Box<dyn StateBackend> = if let Some(config) = parsed.backend.as_ref() {
        let state_config = StateBackendConfig::from(config);
        let backend = create_backend(&state_config)
            .await
            .map_err(|e| format!("Failed to create backend: {}", e))?;

        let bucket_exists = backend
            .bucket_exists()
            .await
            .map_err(|e| format!("Failed to check bucket: {}", e))?;

        if bucket_exists {
            // Try to load state from backend
            state_file = backend
                .read_state()
                .await
                .map_err(|e| format!("Failed to read state: {}", e))?;
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
                        .get("name")
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
                    return Err(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    ));
                }
            }
        }
        backend
    } else {
        // Use local backend by default
        let backend = create_local_backend();
        state_file = backend
            .read_state()
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
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

    reconcile_prefixed_names(&mut parsed.resources, &state_file);

    let ctx = create_plan_from_parsed(&parsed, &state_file).await?;
    let has_changes = ctx.plan.mutation_count() > 0;
    print_plan(&ctx.plan);

    // Save plan to file if --out was specified
    if let Some(out_path) = out {
        let provider_config = parsed
            .providers
            .first()
            .cloned()
            .unwrap_or_else(|| ProviderConfig {
                name: "unknown".to_string(),
                attributes: HashMap::new(),
            });

        let plan_file = PlanFile {
            version: 1,
            carina_version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            source_path: path.display().to_string(),
            state_lineage: state_file.as_ref().map(|s| s.lineage.clone()),
            state_serial: state_file.as_ref().map(|s| s.serial),
            provider_config,
            backend_config: parsed.backend.clone(),
            plan: ctx.plan,
            sorted_resources: ctx.sorted_resources,
            current_states: ctx
                .current_states
                .into_iter()
                .map(|(id, state)| CurrentStateEntry { id, state })
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

async fn run_apply(path: &PathBuf, auto_approve: bool) -> Result<(), String> {
    let loaded = load_configuration(path)?;
    let mut parsed = loaded.parsed;
    let backend_file = loaded.backend_file;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Validate provider region
    validate_provider_region(&parsed)?;

    resolve_names(&mut parsed.resources)?;
    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(|e| format!("Failed to create backend: {}", e))?
    } else {
        create_local_backend()
    };

    // Handle bootstrap if S3 backend is configured
    #[allow(unused_assignments)]
    let mut lock: Option<LockInfo> = None;
    #[allow(unused_assignments)]
    let mut state_file: Option<StateFile> = None;

    if let Some(config) = backend_config {
        // Check if bucket exists (bootstrap detection)
        let bucket_exists = backend
            .bucket_exists()
            .await
            .map_err(|e| format!("Failed to check bucket: {}", e))?;

        if !bucket_exists {
            println!(
                "{}",
                "State bucket not found. Running bootstrap..."
                    .yellow()
                    .bold()
            );

            // Get bucket name from config
            let bucket_name = config
                .attributes
                .get("bucket")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or("Missing bucket name in backend configuration")?;

            // Check if there's a bucket resource defined with matching name
            let backend_resource_type = backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            if let Some(bucket_resource) =
                parsed.find_resource_by_name(backend_resource_type, &bucket_name)
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
                let factories = provider_factories();
                let factory = provider_mod::find_factory(&factories, backend_provider_name)
                    .ok_or_else(|| {
                        format!("No provider factory found for '{}'", backend_provider_name)
                    })?;
                let provider_config_attrs = parsed
                    .providers
                    .iter()
                    .find(|p| p.name == backend_provider_name)
                    .map(|p| p.attributes.clone())
                    .unwrap_or_default();
                let bucket_provider = factory.create_provider(&provider_config_attrs).await;

                match bucket_provider.create(bucket_resource).await {
                    Ok(_) => {
                        println!("  {} Created state bucket: {}", "✓".green(), bucket_name);
                    }
                    Err(e) => {
                        return Err(format!("Failed to create state bucket: {}", e));
                    }
                }
            } else {
                // Auto-create the bucket if auto_create is enabled
                let auto_create = config
                    .attributes
                    .get("auto_create")
                    .and_then(|v| match v {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    })
                    .unwrap_or(true);

                if auto_create {
                    println!("Auto-creating state bucket: {}", bucket_name.cyan());
                    backend
                        .create_bucket()
                        .await
                        .map_err(|e| format!("Failed to create bucket: {}", e))?;
                    println!("  {} Created state bucket", "✓".green());

                    // Get region from backend config using factory
                    let backend_provider_name = backend
                        .provider_name()
                        .ok_or("Backend does not specify a provider name")?;
                    let factories = provider_factories();
                    let factory = provider_mod::find_factory(&factories, backend_provider_name)
                        .ok_or_else(|| {
                            format!("No provider factory found for '{}'", backend_provider_name)
                        })?;
                    let region = factory.extract_region(&config.attributes);

                    // Append resource definition to backend file
                    let target_file = backend_file.clone().unwrap_or_else(|| path.clone());

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

                    // Create a protected ResourceState for the auto-created bucket
                    let backend_resource_type = backend
                        .resource_type()
                        .ok_or("Backend does not specify a resource type")?;
                    let bucket_state = ResourceState::new(
                        backend_resource_type,
                        &bucket_name,
                        backend_provider_name,
                    )
                    .with_attribute("name".to_string(), serde_json::json!(bucket_name))
                    .with_attribute("region".to_string(), serde_json::json!(region))
                    .with_attribute(
                        "versioning_status".to_string(),
                        serde_json::json!("Enabled"),
                    )
                    .with_protected(true);

                    // Initialize state with the protected bucket
                    let mut initial_state = StateFile::new();
                    initial_state.upsert_resource(bucket_state);
                    backend
                        .write_state(&initial_state)
                        .await
                        .map_err(|e| format!("Failed to write initial state: {}", e))?;
                    println!(
                        "  {} Registered state bucket as protected resource",
                        "✓".green()
                    );

                    // Re-parse the updated configuration to include the new resource
                    parsed = load_configuration(path)?.parsed;
                    if let Err(e) =
                        module_resolver::resolve_modules(&mut parsed, get_base_dir(path))
                    {
                        return Err(format!("Module resolution error: {}", e));
                    }
                    resolve_names(&mut parsed.resources)?;
                } else {
                    return Err(format!(
                        "Backend bucket '{}' not found and auto_create is disabled",
                        bucket_name
                    ));
                }
            }

            // Initialize state if not already done (when bucket existed or was created from resource)
            if backend
                .read_state()
                .await
                .map_err(|e| format!("Failed to read state: {}", e))?
                .is_none()
            {
                backend
                    .init()
                    .await
                    .map_err(|e| format!("Failed to initialize state: {}", e))?;
            }
        }

        // Acquire lock
        println!("{}", "Acquiring state lock...".cyan());
        lock = Some(backend.acquire_lock("apply").await.map_err(|e| match e {
            BackendError::Locked {
                who,
                lock_id,
                operation,
            } => {
                format!(
                    "State is locked by {} (lock ID: {}, operation: {})\n\
                            If you believe this is stale, run: carina force-unlock {}",
                    who, lock_id, operation, lock_id
                )
            }
            _ => format!("Failed to acquire lock: {}", e),
        })?);
        println!("  {} Lock acquired", "✓".green());

        // Read current state from backend
        state_file = backend
            .read_state()
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
    } else {
        // Local backend: acquire lock and read state
        println!("{}", "Acquiring state lock...".cyan());
        lock = Some(backend.acquire_lock("apply").await.map_err(|e| match e {
            BackendError::Locked {
                who,
                lock_id,
                operation,
            } => {
                format!(
                    "State is locked by {} (lock ID: {}, operation: {})\n\
                            If you believe this is stale, run: carina force-unlock {}",
                    who, lock_id, operation, lock_id
                )
            }
            _ => format!("Failed to acquire lock: {}", e),
        })?);
        println!("  {} Lock acquired", "✓".green());

        // Read current state from local file
        state_file = backend
            .read_state()
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
    }

    reconcile_prefixed_names(&mut parsed.resources, &state_file);

    // Sort resources by dependencies
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));
        let state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
        current_states.insert(resource.id.clone(), state);
    }

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.restore_unreturned_attrs(&mut current_states, &saved_attrs);

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &sorted_resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();
            // Merge existing state if available
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

    // Resolve references and enum identifiers, then create initial plan for display
    let mut resources_for_plan = sorted_resources.clone();
    resolve_refs_with_state(&mut resources_for_plan, &current_states);
    provider.resolve_enum_identifiers(&mut resources_for_plan);
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();
    let schemas = get_schemas();
    let plan = create_plan(
        &resources_for_plan,
        &current_states,
        &lifecycles,
        &schemas,
        &saved_attrs,
    );

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());

        // Release lock if we have one
        if let Some(lock_info) = &lock {
            backend
                .release_lock(lock_info)
                .await
                .map_err(|e| format!("Failed to release lock: {}", e))?;
        }

        return Ok(());
    }

    print_plan(&plan);

    // Confirmation prompt
    if !auto_approve {
        println!(
            "{}",
            "Do you want to perform these actions?".yellow().bold()
        );
        println!(
            "  {}",
            "Carina will perform the actions described above. Type 'yes' to confirm.".yellow()
        );
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != "yes" {
            println!();
            println!("{}", "Apply cancelled.".yellow());

            // Release lock if we have one
            if let Some(lock_info) = &lock {
                backend
                    .release_lock(lock_info)
                    .await
                    .map_err(|e| format!("Failed to release lock: {}", e))?;
            }

            return Ok(());
        }
        println!();
    }

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();

    // Apply each effect in order, resolving references dynamically
    for effect in plan.effects() {
        // Check if any dependency has failed - skip this effect if so
        if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
            println!(
                "  {} {} - dependency '{}' failed",
                "⊘".yellow(),
                format_effect(effect),
                failed_dep
            );
            skip_count += 1;
            // Propagate failure to this binding so transitive dependents are also skipped
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
            continue;
        }

        match effect {
            Effect::Create(resource) => {
                // Re-resolve references with current binding_map
                let mut resolved_resource = resource.clone();
                for (key, value) in &resource.attributes {
                    resolved_resource
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, &binding_map));
                }

                match provider.create(&resolved_resource).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;

                        // Track the applied state
                        applied_states.insert(resource.id.clone(), state.clone());

                        // Update binding_map with the newly created resource's state (including id)
                        if let Some(Value::String(binding_name)) =
                            resource.attributes.get("_binding")
                        {
                            let mut attrs = resolved_resource.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Update { id, from, to } => {
                // Re-resolve references
                let mut resolved_to = to.clone();
                for (key, value) in &to.attributes {
                    resolved_to
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, &binding_map));
                }

                // Get identifier from current state
                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.update(id, identifier, from, &resolved_to).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;

                        // Track the applied state
                        applied_states.insert(id.clone(), state.clone());

                        // Update binding_map
                        if let Some(Value::String(binding_name)) = to.attributes.get("_binding") {
                            let mut attrs = resolved_to.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Replace {
                id,
                from,
                to,
                lifecycle,
                ..
            } => {
                if lifecycle.create_before_destroy {
                    // Create the new resource first
                    let mut resolved_resource = to.clone();
                    for (key, value) in &to.attributes {
                        resolved_resource
                            .attributes
                            .insert(key.clone(), resolve_ref_value(value, &binding_map));
                    }

                    match provider.create(&resolved_resource).await {
                        Ok(state) => {
                            // Then delete the old resource
                            let identifier = from.identifier.as_deref().unwrap_or("");
                            match provider.delete(id, identifier, lifecycle).await {
                                Ok(()) => {
                                    println!("  {} {}", "✓".green(), format_effect(effect));
                                    success_count += 1;

                                    applied_states.insert(to.id.clone(), state.clone());

                                    if let Some(Value::String(binding_name)) =
                                        to.attributes.get("_binding")
                                    {
                                        let mut attrs = resolved_resource.attributes.clone();
                                        for (k, v) in &state.attributes {
                                            attrs.insert(k.clone(), v.clone());
                                        }
                                        binding_map.insert(binding_name.clone(), attrs);
                                    }
                                }
                                Err(e) => {
                                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                                    failure_count += 1;
                                    if let Some(binding) = effect.binding_name() {
                                        failed_bindings.insert(binding);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                } else {
                    // Delete the existing resource first
                    let identifier = from.identifier.as_deref().unwrap_or("");
                    match provider.delete(id, identifier, lifecycle).await {
                        Ok(()) => {
                            // Re-resolve references with current binding_map
                            let mut resolved_resource = to.clone();
                            for (key, value) in &to.attributes {
                                resolved_resource
                                    .attributes
                                    .insert(key.clone(), resolve_ref_value(value, &binding_map));
                            }

                            // Create the new resource
                            match provider.create(&resolved_resource).await {
                                Ok(state) => {
                                    println!("  {} {}", "✓".green(), format_effect(effect));
                                    success_count += 1;

                                    applied_states.insert(to.id.clone(), state.clone());

                                    if let Some(Value::String(binding_name)) =
                                        to.attributes.get("_binding")
                                    {
                                        let mut attrs = resolved_resource.attributes.clone();
                                        for (k, v) in &state.attributes {
                                            attrs.insert(k.clone(), v.clone());
                                        }
                                        binding_map.insert(binding_name.clone(), attrs);
                                    }
                                }
                                Err(e) => {
                                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                                    failure_count += 1;
                                    if let Some(binding) = effect.binding_name() {
                                        failed_bindings.insert(binding);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                }
            }
            Effect::Delete {
                id,
                identifier,
                lifecycle,
            } => match provider.delete(id, identifier, lifecycle).await {
                Ok(()) => {
                    println!("  {} {}", "✓".green(), format_effect(effect));
                    success_count += 1;
                    successfully_deleted.insert(id.clone());
                }
                Err(e) => {
                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                    failure_count += 1;
                }
            },
            Effect::Read { .. } => {}
        }
    }

    // Save state
    println!();
    println!("{}", "Saving state...".cyan());

    // Get or create state file
    let mut state = state_file.unwrap_or_default();

    // Update state with current resources
    for resource in &sorted_resources {
        let existing = state.find_resource(&resource.id.resource_type, &resource.id.name);
        if let Some(applied_state) = applied_states.get(&resource.id) {
            let resource_state =
                ResourceState::from_provider_state(resource, applied_state, existing);
            state.upsert_resource(resource_state);
        } else if let Some(current_state) = current_states.get(&resource.id)
            && current_state.exists
        {
            let resource_state =
                ResourceState::from_provider_state(resource, current_state, existing);
            state.upsert_resource(resource_state);
        }
    }

    // Remove only successfully deleted resources from state
    for effect in plan.effects() {
        if let Effect::Delete { id, .. } = effect
            && successfully_deleted.contains(id)
        {
            state.remove_resource(&id.resource_type, &id.name);
        }
    }

    // Increment serial and save
    state.increment_serial();
    backend
        .write_state(&state)
        .await
        .map_err(|e| format!("Failed to write state: {}", e))?;
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    // Release lock
    if let Some(ref lock_info) = lock {
        backend
            .release_lock(lock_info)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        println!("  {} Lock released", "✓".green());
    }

    println!();
    if failure_count == 0 && skip_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        let mut parts = vec![format!("{} succeeded", success_count)];
        if failure_count > 0 {
            parts.push(format!("{} failed", failure_count));
        }
        if skip_count > 0 {
            parts.push(format!("{} skipped", skip_count));
        }
        Err(format!("Apply failed. {}.", parts.join(", ")))
    }
}

async fn run_apply_from_plan(plan_path: &PathBuf, auto_approve: bool) -> Result<(), String> {
    // Read and deserialize the plan file
    let content =
        fs::read_to_string(plan_path).map_err(|e| format!("Failed to read plan file: {}", e))?;
    let plan_file: PlanFile =
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse plan file: {}", e))?;

    // Validate version compatibility
    if plan_file.version != 1 {
        return Err(format!(
            "Unsupported plan file version: {} (expected 1)",
            plan_file.version
        ));
    }

    let current_version = env!("CARGO_PKG_VERSION");
    if plan_file.carina_version != current_version {
        println!(
            "{}",
            format!(
                "Warning: plan was created with carina {} but current version is {}",
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

    // Set up backend
    let backend: Box<dyn StateBackend> = if let Some(config) = plan_file.backend_config.as_ref() {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(|e| format!("Failed to create backend: {}", e))?
    } else {
        create_local_backend()
    };

    // Acquire lock
    println!("{}", "Acquiring state lock...".cyan());
    let lock = backend.acquire_lock("apply").await.map_err(|e| match e {
        BackendError::Locked {
            who,
            lock_id,
            operation,
        } => {
            format!(
                "State is locked by {} (lock ID: {}, operation: {})\n\
                        If you believe this is stale, run: carina force-unlock {}",
                who, lock_id, operation, lock_id
            )
        }
        _ => format!("Failed to acquire lock: {}", e),
    })?;
    println!("  {} Lock acquired", "✓".green());

    // Read current state and validate lineage
    let state_file = backend
        .read_state()
        .await
        .map_err(|e| format!("Failed to read state: {}", e))?;

    if let Some(ref state) = state_file {
        // Validate state lineage
        if let Some(ref plan_lineage) = plan_file.state_lineage
            && &state.lineage != plan_lineage
        {
            backend
                .release_lock(&lock)
                .await
                .map_err(|e| format!("Failed to release lock: {}", e))?;
            return Err(format!(
                "State lineage mismatch: plan was created for lineage '{}' but current state has '{}'",
                plan_lineage, state.lineage
            ));
        }

        // Warn on serial mismatch (state may have drifted)
        if let Some(plan_serial) = plan_file.state_serial
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

    let plan = &plan_file.plan;
    let sorted_resources = &plan_file.sorted_resources;

    // Rebuild planned current_states HashMap from plan file
    let planned_states: HashMap<ResourceId, State> = plan_file
        .current_states
        .into_iter()
        .map(|entry| (entry.id, entry.state))
        .collect();

    // Create provider early for drift detection
    let provider: Box<dyn Provider> = create_provider_from_config(&plan_file.provider_config).await;

    // Drift detection: re-read actual infrastructure state and compare against planned states
    println!("{}", "Checking for infrastructure drift...".cyan());
    let mut drift_detected = false;
    let mut drift_messages: Vec<String> = Vec::new();

    for resource in sorted_resources {
        let planned_state = planned_states.get(&resource.id);
        let identifier = planned_state.and_then(|s| s.identifier.as_deref());

        let actual_state = provider
            .read(&resource.id, identifier)
            .await
            .map_err(|e| format!("Failed to read current state of {}: {}", resource.id, e))?;

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
                // Compare attributes for existing resources
                let mut attr_diffs: Vec<String> = Vec::new();
                for (key, planned_val) in &planned.attributes {
                    if key.starts_with('_') {
                        continue;
                    }
                    match actual_state.attributes.get(key) {
                        Some(actual_val) if actual_val != planned_val => {
                            attr_diffs.push(format!(
                                "      {}: {} → {}",
                                key,
                                format_value(planned_val),
                                format_value(actual_val)
                            ));
                        }
                        None => {
                            attr_diffs.push(format!(
                                "      {}: {} → (removed)",
                                key,
                                format_value(planned_val)
                            ));
                        }
                        _ => {}
                    }
                }
                for (key, actual_val) in &actual_state.attributes {
                    if key.starts_with('_') {
                        continue;
                    }
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
            }
        }
    }

    if drift_detected {
        println!();
        println!("{}", "Error: Infrastructure drift detected!".red().bold());
        println!(
            "{}",
            "The following resources have changed since the plan was created:".red()
        );
        println!();
        for msg in &drift_messages {
            println!("{}", msg);
        }
        println!();
        println!(
            "{}",
            "Please re-run 'carina plan' to create a new plan that reflects the current state."
                .yellow()
        );
        backend
            .release_lock(&lock)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        return Err("Apply aborted due to infrastructure drift.".to_string());
    }

    println!("  {} No drift detected.", "✓".green());

    // Use the actual states (freshly read) as current_states for apply
    let current_states = planned_states;

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());
        backend
            .release_lock(&lock)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        return Ok(());
    }

    print_plan(plan);

    // Confirmation prompt
    if !auto_approve {
        println!(
            "{}",
            "Do you want to perform these actions?".yellow().bold()
        );
        println!(
            "  {}",
            "Carina will perform the actions described above. Type 'yes' to confirm.".yellow()
        );
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != "yes" {
            println!();
            println!("{}", "Apply cancelled.".yellow());
            backend
                .release_lock(&lock)
                .await
                .map_err(|e| format!("Failed to release lock: {}", e))?;
            return Ok(());
        }
        println!();
    }

    // Build initial binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in sorted_resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();
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

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    let mut successfully_deleted: HashSet<ResourceId> = HashSet::new();

    // Apply each effect in order, resolving references dynamically
    for effect in plan.effects() {
        // Check if any dependency has failed - skip this effect if so
        if let Some(failed_dep) = find_failed_dependency(effect, &failed_bindings) {
            println!(
                "  {} {} - dependency '{}' failed",
                "⊘".yellow(),
                format_effect(effect),
                failed_dep
            );
            skip_count += 1;
            // Propagate failure to this binding so transitive dependents are also skipped
            if let Some(binding) = effect.binding_name() {
                failed_bindings.insert(binding);
            }
            continue;
        }

        match effect {
            Effect::Create(resource) => {
                let mut resolved_resource = resource.clone();
                for (key, value) in &resource.attributes {
                    resolved_resource
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, &binding_map));
                }

                match provider.create(&resolved_resource).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;
                        applied_states.insert(resource.id.clone(), state.clone());

                        if let Some(Value::String(binding_name)) =
                            resource.attributes.get("_binding")
                        {
                            let mut attrs = resolved_resource.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Update { id, from, to } => {
                let mut resolved_to = to.clone();
                for (key, value) in &to.attributes {
                    resolved_to
                        .attributes
                        .insert(key.clone(), resolve_ref_value(value, &binding_map));
                }

                let identifier = from.identifier.as_deref().unwrap_or("");
                match provider.update(id, identifier, from, &resolved_to).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;
                        applied_states.insert(id.clone(), state.clone());

                        if let Some(Value::String(binding_name)) = to.attributes.get("_binding") {
                            let mut attrs = resolved_to.attributes.clone();
                            for (k, v) in &state.attributes {
                                attrs.insert(k.clone(), v.clone());
                            }
                            binding_map.insert(binding_name.clone(), attrs);
                        }
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                        if let Some(binding) = effect.binding_name() {
                            failed_bindings.insert(binding);
                        }
                    }
                }
            }
            Effect::Replace {
                id,
                from,
                to,
                lifecycle,
                ..
            } => {
                if lifecycle.create_before_destroy {
                    // Create the new resource first
                    let mut resolved_resource = to.clone();
                    for (key, value) in &to.attributes {
                        resolved_resource
                            .attributes
                            .insert(key.clone(), resolve_ref_value(value, &binding_map));
                    }

                    match provider.create(&resolved_resource).await {
                        Ok(state) => {
                            // Then delete the old resource
                            let identifier = from.identifier.as_deref().unwrap_or("");
                            match provider.delete(id, identifier, lifecycle).await {
                                Ok(()) => {
                                    println!("  {} {}", "✓".green(), format_effect(effect));
                                    success_count += 1;
                                    applied_states.insert(to.id.clone(), state.clone());

                                    if let Some(Value::String(binding_name)) =
                                        to.attributes.get("_binding")
                                    {
                                        let mut attrs = resolved_resource.attributes.clone();
                                        for (k, v) in &state.attributes {
                                            attrs.insert(k.clone(), v.clone());
                                        }
                                        binding_map.insert(binding_name.clone(), attrs);
                                    }
                                }
                                Err(e) => {
                                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                                    failure_count += 1;
                                    if let Some(binding) = effect.binding_name() {
                                        failed_bindings.insert(binding);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                } else {
                    let identifier = from.identifier.as_deref().unwrap_or("");
                    match provider.delete(id, identifier, lifecycle).await {
                        Ok(()) => {
                            let mut resolved_resource = to.clone();
                            for (key, value) in &to.attributes {
                                resolved_resource
                                    .attributes
                                    .insert(key.clone(), resolve_ref_value(value, &binding_map));
                            }

                            match provider.create(&resolved_resource).await {
                                Ok(state) => {
                                    println!("  {} {}", "✓".green(), format_effect(effect));
                                    success_count += 1;
                                    applied_states.insert(to.id.clone(), state.clone());

                                    if let Some(Value::String(binding_name)) =
                                        to.attributes.get("_binding")
                                    {
                                        let mut attrs = resolved_resource.attributes.clone();
                                        for (k, v) in &state.attributes {
                                            attrs.insert(k.clone(), v.clone());
                                        }
                                        binding_map.insert(binding_name.clone(), attrs);
                                    }
                                }
                                Err(e) => {
                                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                                    failure_count += 1;
                                    if let Some(binding) = effect.binding_name() {
                                        failed_bindings.insert(binding);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                            failure_count += 1;
                            if let Some(binding) = effect.binding_name() {
                                failed_bindings.insert(binding);
                            }
                        }
                    }
                }
            }
            Effect::Delete {
                id,
                identifier,
                lifecycle,
            } => match provider.delete(id, identifier, lifecycle).await {
                Ok(()) => {
                    println!("  {} {}", "✓".green(), format_effect(effect));
                    success_count += 1;
                    successfully_deleted.insert(id.clone());
                }
                Err(e) => {
                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                    failure_count += 1;
                }
            },
            Effect::Read { .. } => {}
        }
    }

    // Save state
    println!();
    println!("{}", "Saving state...".cyan());

    let mut state = state_file.unwrap_or_default();

    for resource in sorted_resources {
        let existing = state.find_resource(&resource.id.resource_type, &resource.id.name);
        if let Some(applied_state) = applied_states.get(&resource.id) {
            let resource_state =
                ResourceState::from_provider_state(resource, applied_state, existing);
            state.upsert_resource(resource_state);
        } else if let Some(current_state) = current_states.get(&resource.id)
            && current_state.exists
        {
            let resource_state =
                ResourceState::from_provider_state(resource, current_state, existing);
            state.upsert_resource(resource_state);
        }
    }

    // Remove only successfully deleted resources from state
    for effect in plan.effects() {
        if let Effect::Delete { id, .. } = effect
            && successfully_deleted.contains(id)
        {
            state.remove_resource(&id.resource_type, &id.name);
        }
    }

    // Increment serial and save
    state.increment_serial();
    backend
        .write_state(&state)
        .await
        .map_err(|e| format!("Failed to write state: {}", e))?;
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    // Release lock
    backend
        .release_lock(&lock)
        .await
        .map_err(|e| format!("Failed to release lock: {}", e))?;
    println!("  {} Lock released", "✓".green());

    println!();
    if failure_count == 0 && skip_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        let mut parts = vec![format!("{} succeeded", success_count)];
        if failure_count > 0 {
            parts.push(format!("{} failed", failure_count));
        }
        if skip_count > 0 {
            parts.push(format!("{} skipped", skip_count));
        }
        Err(format!("Apply failed. {}.", parts.join(", ")))
    }
}

/// Create a provider from a saved ProviderConfig
async fn run_destroy(path: &PathBuf, auto_approve: bool) -> Result<(), String> {
    let mut parsed = load_configuration(path)?.parsed;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Validate provider region
    validate_provider_region(&parsed)?;

    resolve_names(&mut parsed.resources)?;
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    if parsed.resources.is_empty() {
        println!("{}", "No resources defined in configuration.".yellow());
        return Ok(());
    }

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(|e| format!("Failed to create backend: {}", e))?
    } else {
        create_local_backend()
    };

    // Handle state locking
    #[allow(unused_assignments)]
    let mut lock: Option<LockInfo> = None;
    #[allow(unused_assignments)]
    let mut state_file: Option<StateFile> = None;
    let mut protected_bucket: Option<String> = None;

    // Get the state bucket name for protection check (S3 backend only)
    if let Some(config) = backend_config {
        protected_bucket = config.attributes.get("bucket").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });
    }

    // Acquire lock
    println!("{}", "Acquiring state lock...".cyan());
    lock = Some(backend.acquire_lock("destroy").await.map_err(|e| match e {
        BackendError::Locked {
            who,
            lock_id,
            operation,
        } => {
            format!(
                "State is locked by {} (lock ID: {}, operation: {})\n\
                        If you believe this is stale, run: carina force-unlock {}",
                who, lock_id, operation, lock_id
            )
        }
        _ => format!("Failed to acquire lock: {}", e),
    })?);
    println!("  {} Lock acquired", "✓".green());

    // Read current state from backend
    state_file = backend
        .read_state()
        .await
        .map_err(|e| format!("Failed to read state: {}", e))?;

    reconcile_prefixed_names(&mut parsed.resources, &state_file);

    // Sort resources by dependencies (for creation order)
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Reverse the order for destruction (dependents first, then dependencies)
    let destroy_order: Vec<Resource> = sorted_resources.into_iter().rev().collect();

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Read states for managed resources using identifier from state
    // Skip data sources (read-only) — they won't be destroyed
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &destroy_order {
        if resource.read_only {
            continue;
        }
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));
        let state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
        current_states.insert(resource.id.clone(), state);
    }

    // Collect resources that exist and will be destroyed
    // Skip the state bucket if it matches the backend bucket
    let mut protected_resources: Vec<&Resource> = Vec::new();
    let resources_to_destroy: Vec<&Resource> = destroy_order
        .iter()
        .filter(|r| {
            // Skip data sources (read-only resources) — nothing to destroy
            if r.read_only {
                return false;
            }

            if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
                return false;
            }

            // Check if this is the protected state bucket
            if let Some(backend_rt) = backend.resource_type()
                && r.id.resource_type == backend_rt
                && let Some(ref bucket_name) = protected_bucket
                && let Some(Value::String(name)) = r.attributes.get("name")
                && name == bucket_name
            {
                protected_resources.push(r);
                return false;
            }

            true
        })
        .collect();

    if resources_to_destroy.is_empty() && protected_resources.is_empty() {
        println!("{}", "No resources to destroy.".green());

        // Release lock if we have one
        if let Some(lock_info) = &lock {
            backend
                .release_lock(lock_info)
                .await
                .map_err(|e| format!("Failed to release lock: {}", e))?;
        }

        return Ok(());
    }

    // Display destroy plan
    println!("{}", "Destroy Plan:".red().bold());
    println!();

    for resource in &resources_to_destroy {
        println!("  {} {}", "-".red().bold(), resource.id);
    }

    // Show protected resources
    for resource in &protected_resources {
        println!(
            "  {} {} {}",
            "⚠".yellow().bold(),
            resource.id,
            "(protected - will be skipped)".yellow()
        );
    }

    println!();
    let total_count = resources_to_destroy.len() + protected_resources.len();
    if !protected_resources.is_empty() {
        println!(
            "Plan: {} to destroy, {} protected.",
            resources_to_destroy.len().to_string().red(),
            protected_resources.len().to_string().yellow()
        );
    } else {
        println!("Plan: {} to destroy.", total_count.to_string().red());
    }
    println!();

    if resources_to_destroy.is_empty() {
        println!(
            "{}",
            "All resources are protected. Nothing to destroy.".yellow()
        );

        // Release lock if we have one
        if let Some(lock_info) = &lock {
            backend
                .release_lock(lock_info)
                .await
                .map_err(|e| format!("Failed to release lock: {}", e))?;
        }

        return Ok(());
    }

    // Confirmation prompt
    if !auto_approve {
        println!(
            "{}",
            "Do you really want to destroy all resources?"
                .yellow()
                .bold()
        );
        println!(
            "  {}",
            "This action cannot be undone. Type 'yes' to confirm.".yellow()
        );
        print!("\n  Enter a value: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != "yes" {
            println!();
            println!("{}", "Destroy cancelled.".yellow());

            // Release lock if we have one
            if let Some(lock_info) = &lock {
                backend
                    .release_lock(lock_info)
                    .await
                    .map_err(|e| format!("Failed to release lock: {}", e))?;
            }

            return Ok(());
        }
        println!();
    }

    println!("{}", "Destroying resources...".red().bold());
    println!();

    // Build reverse dependency map for wait-for-completion logic
    let dependents_map = build_dependents_map(&resources_to_destroy);

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut skip_count = 0;
    let mut destroyed_ids: Vec<ResourceId> = Vec::new();
    let mut failed_bindings: HashSet<String> = HashSet::new();
    // timed_out_resources: binding -> (ResourceId, identifier)
    let mut timed_out_resources: HashMap<String, (ResourceId, String)> = HashMap::new();

    for resource in &resources_to_destroy {
        let identifier = current_states
            .get(&resource.id)
            .and_then(|s| s.identifier.clone())
            .unwrap_or_default();
        let effect = Effect::Delete {
            id: resource.id.clone(),
            identifier: identifier.clone(),
            lifecycle: resource.lifecycle.clone(),
        };

        let binding = resource
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));

        // Check if any dependent has actually failed (non-timeout)
        if let Some(failed_dep) = find_failed_dependent(&binding, &dependents_map, &failed_bindings)
        {
            println!(
                "  {} {} - skipped (dependent {} failed)",
                "⊘".yellow(),
                format_effect(&effect),
                failed_dep
            );
            skip_count += 1;
            continue;
        }

        // Check if any dependent timed out — wait for it to complete
        let timed_out_deps: Vec<String> = dependents_map
            .get(&binding)
            .map(|deps| {
                deps.iter()
                    .filter(|d| timed_out_resources.contains_key(d.as_str()))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();

        let mut wait_failed = false;
        for dep_binding in &timed_out_deps {
            if let Some((dep_id, dep_identifier)) = timed_out_resources.remove(dep_binding.as_str())
            {
                println!(
                    "  {} Waiting for {} to be deleted...",
                    "⏳".yellow(),
                    dep_id
                );

                let mut completed = false;
                for _ in 0..180 {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    match provider.read(&dep_id, Some(&dep_identifier)).await {
                        Ok(state) if !state.exists => {
                            println!(
                                "  {} Delete {} (completed after extended wait)",
                                "✓".green(),
                                dep_id
                            );
                            destroyed_ids.push(dep_id.clone());
                            success_count += 1;
                            completed = true;
                            break;
                        }
                        Ok(_) => {
                            // Still exists, keep waiting
                        }
                        Err(_) => {
                            // Read error — resource may be gone, treat as completed
                            println!(
                                "  {} Delete {} (completed after extended wait)",
                                "✓".green(),
                                dep_id
                            );
                            destroyed_ids.push(dep_id.clone());
                            success_count += 1;
                            completed = true;
                            break;
                        }
                    }
                }

                if !completed {
                    println!(
                        "  {} {} - still exists after extended wait",
                        "✗".red(),
                        dep_id
                    );
                    failed_bindings.insert(dep_binding.clone());
                    failure_count += 1;
                    wait_failed = true;
                }
            }
        }

        if wait_failed {
            println!(
                "  {} {} - skipped (dependent deletion did not complete)",
                "⊘".yellow(),
                format_effect(&effect)
            );
            skip_count += 1;
            continue;
        }

        let delete_result = provider
            .delete(&resource.id, &identifier, &resource.lifecycle)
            .await;

        match delete_result {
            Ok(()) => {
                println!("  {} {}", "✓".green(), format_effect(&effect));
                success_count += 1;
                destroyed_ids.push(resource.id.clone());
            }
            Err(e) if e.is_timeout => {
                println!(
                    "  {} {} - Operation timed out, waiting for completion...",
                    "⏳".yellow(),
                    format_effect(&effect)
                );
                timed_out_resources
                    .insert(binding.clone(), (resource.id.clone(), identifier.clone()));
            }
            Err(e) => {
                println!("  {} {} - {}", "✗".red(), format_effect(&effect), e);
                failure_count += 1;
                failed_bindings.insert(binding.clone());
            }
        }
    }

    // Handle any remaining timed-out resources that no parent waited on
    for (dep_binding, (dep_id, dep_identifier)) in &timed_out_resources {
        println!(
            "  {} Waiting for {} to be deleted...",
            "⏳".yellow(),
            dep_id
        );

        let mut completed = false;
        for _ in 0..180 {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            match provider.read(dep_id, Some(dep_identifier)).await {
                Ok(state) if !state.exists => {
                    println!(
                        "  {} Delete {} (completed after extended wait)",
                        "✓".green(),
                        dep_id
                    );
                    destroyed_ids.push(dep_id.clone());
                    success_count += 1;
                    completed = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    println!(
                        "  {} Delete {} (completed after extended wait)",
                        "✓".green(),
                        dep_id
                    );
                    destroyed_ids.push(dep_id.clone());
                    success_count += 1;
                    completed = true;
                    break;
                }
            }
        }

        if !completed {
            println!(
                "  {} {} - still exists after extended wait",
                "✗".red(),
                dep_id
            );
            failed_bindings.insert(dep_binding.clone());
            failure_count += 1;
        }
    }

    // Save state
    println!();
    println!("{}", "Saving state...".cyan());

    // Get or create state file
    let mut state = state_file.unwrap_or_default();

    // Remove destroyed resources from state
    for id in &destroyed_ids {
        state.remove_resource(&id.resource_type, &id.name);
    }

    // Increment serial and save
    state.increment_serial();
    backend
        .write_state(&state)
        .await
        .map_err(|e| format!("Failed to write state: {}", e))?;
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    // Release lock
    if let Some(ref lock_info) = lock {
        backend
            .release_lock(lock_info)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        println!("  {} Lock released", "✓".green());
    }

    println!();
    if failure_count == 0 && skip_count == 0 {
        println!(
            "{}",
            format!("Destroy complete! {} resources destroyed.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        Err(format!(
            "Destroy failed. {} succeeded, {} failed, {} skipped.",
            success_count, failure_count, skip_count
        ))
    }
}

// =============================================================================
// State Management Functions
// =============================================================================

/// Run force-unlock command
async fn run_force_unlock(lock_id: &str, path: &PathBuf) -> Result<(), String> {
    let parsed = load_configuration(path)?.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found. force-unlock requires a backend.")?;

    let state_config = StateBackendConfig::from(backend_config);
    let backend = create_backend(&state_config)
        .await
        .map_err(|e| format!("Failed to create backend: {}", e))?;

    println!("{}", "Force unlocking state...".yellow().bold());
    println!("Lock ID: {}", lock_id);

    match backend.force_unlock(lock_id).await {
        Ok(()) => {
            println!("{}", "State has been successfully unlocked.".green().bold());
            Ok(())
        }
        Err(BackendError::LockNotFound(_)) => Err(format!("Lock with ID '{}' not found.", lock_id)),
        Err(BackendError::LockMismatch { expected, actual }) => Err(format!(
            "Lock ID mismatch. Expected '{}', found '{}'.",
            expected, actual
        )),
        Err(e) => Err(format!("Failed to force unlock: {}", e)),
    }
}

/// Run state subcommands
async fn run_state_command(command: StateCommands) -> Result<(), String> {
    match command {
        StateCommands::BucketDelete {
            bucket_name,
            force,
            path,
        } => run_state_bucket_delete(&bucket_name, force, &path).await,
        StateCommands::Refresh { path } => run_state_refresh(&path).await,
    }
}

/// Run state bucket delete command
async fn run_state_bucket_delete(
    bucket_name: &str,
    force: bool,
    path: &PathBuf,
) -> Result<(), String> {
    let parsed = load_configuration(path)?.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found.")?;

    // Verify the bucket name matches the backend configuration
    let config_bucket = backend_config
        .attributes
        .get("bucket")
        .and_then(|v| match v {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        })
        .ok_or("Backend configuration missing 'bucket' attribute")?;

    if config_bucket != bucket_name {
        return Err(format!(
            "Bucket name '{}' does not match backend configuration bucket '{}'.",
            bucket_name, config_bucket
        ));
    }

    println!(
        "{}",
        "WARNING: This will delete the state bucket and all state history."
            .red()
            .bold()
    );
    println!("Bucket: {}", bucket_name.yellow());

    if !force {
        println!();
        println!("{}", "Type the bucket name to confirm deletion:".yellow());
        print!("  Enter bucket name: ");
        std::io::Write::flush(&mut std::io::stdout()).map_err(|e| e.to_string())?;

        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| e.to_string())?;

        if input.trim() != bucket_name {
            println!();
            println!("{}", "Deletion cancelled.".yellow());
            return Ok(());
        }
    }

    // Create backend to get provider metadata
    let state_config = StateBackendConfig::from(backend_config);
    let backend = create_backend(&state_config)
        .await
        .map_err(|e| format!("Failed to create backend: {}", e))?;

    // Get provider metadata from backend
    let backend_provider_name = backend
        .provider_name()
        .ok_or("Backend does not specify a provider name")?;
    let backend_resource_type = backend
        .resource_type()
        .ok_or("Backend does not specify a resource type")?;
    let factories = provider_factories();
    let factory = provider_mod::find_factory(&factories, backend_provider_name)
        .ok_or_else(|| format!("No provider factory found for '{}'", backend_provider_name))?;

    // Create provider to delete the bucket
    let provider_config_attrs = parsed
        .providers
        .iter()
        .find(|p| p.name == backend_provider_name)
        .map(|p| p.attributes.clone())
        .unwrap_or_default();
    let bucket_provider = factory.create_provider(&provider_config_attrs).await;

    // First, try to empty the bucket (delete all objects and versions)
    println!();
    println!("{}", "Emptying bucket...".cyan());

    // Delete the bucket resource (identifier is the bucket name)
    let bucket_id =
        ResourceId::with_provider(backend_provider_name, backend_resource_type, bucket_name);
    match bucket_provider
        .delete(&bucket_id, bucket_name, &LifecycleConfig::default())
        .await
    {
        Ok(()) => {
            println!(
                "{}",
                format!("Deleted state bucket: {}", bucket_name)
                    .green()
                    .bold()
            );
            Ok(())
        }
        Err(e) => Err(format!("Failed to delete bucket: {}", e)),
    }
}

/// Run state refresh command
async fn run_state_refresh(path: &PathBuf) -> Result<(), String> {
    let loaded = load_configuration(path)?;
    let mut parsed = loaded.parsed;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    resolve_names(&mut parsed.resources)?;
    compute_anonymous_identifiers(&mut parsed.resources, &parsed.providers)?;

    // Create backend
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = StateBackendConfig::from(config);
        create_backend(&state_config)
            .await
            .map_err(|e| format!("Failed to create backend: {}", e))?
    } else {
        create_local_backend()
    };

    // Acquire lock
    println!("{}", "Acquiring state lock...".cyan());
    let lock = backend.acquire_lock("refresh").await.map_err(|e| match e {
        BackendError::Locked {
            who,
            lock_id,
            operation,
        } => {
            format!(
                "State is locked by {} (lock ID: {}, operation: {})\n\
                 If you believe this is stale, run: carina force-unlock {}",
                who, lock_id, operation, lock_id
            )
        }
        _ => format!("Failed to acquire lock: {}", e),
    })?;
    println!("  {} Lock acquired", "✓".green());

    // Read current state from backend
    let state_file = backend
        .read_state()
        .await
        .map_err(|e| format!("Failed to read state: {}", e))?;

    let Some(mut state) = state_file else {
        println!("{}", "No state file found. Nothing to refresh.".yellow());
        backend
            .release_lock(&lock)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        return Ok(());
    };

    if state.resources.is_empty() {
        println!("{}", "No resources in state. Nothing to refresh.".yellow());
        backend
            .release_lock(&lock)
            .await
            .map_err(|e| format!("Failed to release lock: {}", e))?;
        return Ok(());
    }

    reconcile_prefixed_names(&mut parsed.resources, &Some(state.clone()));

    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Select provider
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    println!();
    println!("{}", "Refreshing state...".cyan().bold());
    println!();

    let mut updated_count = 0u32;
    let mut unchanged_count = 0u32;

    for resource in &sorted_resources {
        let identifier = state.get_identifier_for_resource(resource);
        let identifier_str = identifier.as_deref();

        // Skip resources not in state (no identifier means not managed)
        if identifier_str.is_none() {
            continue;
        }

        let new_state = provider
            .read(&resource.id, identifier_str)
            .await
            .map_err(|e| format!("Failed to read state for {}: {}", resource.id, e))?;

        // Compare old state attributes with new
        let existing = state.find_resource(&resource.id.resource_type, &resource.id.name);

        let mut has_changes = false;
        let mut changes: Vec<String> = Vec::new();

        if let Some(existing_rs) = existing {
            // Build old attributes as DSL values for comparison
            let old_attrs: HashMap<String, Value> = existing_rs
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), json_to_dsl_value(v)))
                .collect();

            if !new_state.exists {
                // Resource was deleted externally
                has_changes = true;
                changes.push(format!("    {} resource no longer exists", "-".red()));
            } else {
                // Check for modified and removed attributes
                let mut all_keys: HashSet<&String> = old_attrs.keys().collect();
                all_keys.extend(new_state.attributes.keys());

                let mut sorted_keys: Vec<&&String> = all_keys.iter().collect();
                sorted_keys.sort();

                for key in sorted_keys {
                    let old_val = old_attrs.get(*key);
                    let new_val = new_state.attributes.get(*key);

                    match (old_val, new_val) {
                        (Some(old), Some(new)) if old != new => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {} {} {}",
                                "~".yellow(),
                                key,
                                format_value(old).red(),
                                "→".dimmed(),
                                format_value(new).green(),
                            ));
                        }
                        (Some(old), None) => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {}",
                                "-".red(),
                                key,
                                format_value(old).red(),
                            ));
                        }
                        (None, Some(new)) => {
                            has_changes = true;
                            changes.push(format!(
                                "    {} {}: {}",
                                "+".green(),
                                key,
                                format_value(new).green(),
                            ));
                        }
                        _ => {}
                    }
                }
            }
        } else {
            // Resource in config but not in state — shouldn't happen during refresh
            continue;
        }

        if has_changes {
            updated_count += 1;
            println!(
                "  {} \"{}\":",
                resource.id.resource_type.cyan(),
                resource.id.name
            );
            for change in &changes {
                println!("{}", change);
            }
            println!();
        } else {
            unchanged_count += 1;
        }

        // Update state with refreshed data
        if new_state.exists {
            let existing_rs = state.find_resource(&resource.id.resource_type, &resource.id.name);
            let resource_state =
                ResourceState::from_provider_state(resource, &new_state, existing_rs);
            state.upsert_resource(resource_state);
        } else {
            state.remove_resource(&resource.id.resource_type, &resource.id.name);
        }
    }

    // Save updated state
    state.increment_serial();
    backend
        .write_state(&state)
        .await
        .map_err(|e| format!("Failed to write state: {}", e))?;

    // Release lock
    backend
        .release_lock(&lock)
        .await
        .map_err(|e| format!("Failed to release lock: {}", e))?;

    // Summary
    println!(
        "State refreshed: {} resource{} updated, {} resource{} unchanged.",
        updated_count,
        if updated_count == 1 { "" } else { "s" },
        unchanged_count,
        if unchanged_count == 1 { "" } else { "s" },
    );
    println!("  {} State saved (serial: {})", "✓".green(), state.serial);

    Ok(())
}

// Format command implementation
fn run_fmt(path: &PathBuf, check: bool, show_diff: bool, recursive: bool) -> Result<(), String> {
    let config = FormatConfig::default();

    let files = if path.is_file() {
        vec![path.clone()]
    } else if recursive {
        find_crn_files_recursive(path)?
    } else {
        find_crn_files_in_dir(path)?
    };

    if files.is_empty() {
        println!("{}", "No .crn files found.".yellow());
        return Ok(());
    }

    let mut needs_formatting = Vec::new();
    let mut errors = Vec::new();

    for file in &files {
        let content = fs::read_to_string(file)
            .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

        match formatter::format(&content, &config) {
            Ok(formatted) => {
                if content != formatted {
                    needs_formatting.push((file.clone(), content.clone(), formatted.clone()));

                    if show_diff {
                        print_diff(file, &content, &formatted);
                    }

                    if !check {
                        fs::write(file, &formatted)
                            .map_err(|e| format!("Failed to write {}: {}", file.display(), e))?;
                        println!("{} {}", "Formatted:".green(), file.display());
                    }
                }
            }
            Err(e) => {
                errors.push((file.clone(), e.to_string()));
            }
        }
    }

    // Print summary
    if check {
        if needs_formatting.is_empty() && errors.is_empty() {
            println!("{}", "All files are properly formatted.".green());
            Ok(())
        } else {
            if !needs_formatting.is_empty() {
                println!("{}", "The following files need formatting:".yellow());
                for (file, _, _) in &needs_formatting {
                    println!("  {}", file.display());
                }
            }
            for (file, err) in &errors {
                eprintln!("{} {}: {}", "Error:".red(), file.display(), err);
            }
            Err("Some files are not properly formatted".to_string())
        }
    } else if !errors.is_empty() {
        for (file, err) in &errors {
            eprintln!("{} {}: {}", "Error:".red(), file.display(), err);
        }
        Err("Some files had formatting errors".to_string())
    } else {
        let count = needs_formatting.len();
        if count > 0 {
            println!("{}", format!("Formatted {} file(s).", count).green().bold());
        } else {
            println!("{}", "All files are already properly formatted.".green());
        }
        Ok(())
    }
}

fn print_diff(file: &Path, original: &str, formatted: &str) {
    println!("\n{} {}:", "Diff for".cyan().bold(), file.display());

    let diff = TextDiff::from_lines(original, formatted);
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-".red(),
            ChangeTag::Insert => "+".green(),
            ChangeTag::Equal => " ".normal(),
        };
        print!("{}{}", sign, change);
    }
}

/// A lint warning with file, line, and message info.
struct LintWarning {
    file: PathBuf,
    line: usize,
    message: String,
}

fn run_lint(path: &PathBuf) -> Result<(), String> {
    let mut parsed = load_configuration(path)?.parsed;

    let base_dir = get_base_dir(path);

    // Resolve modules
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    let factories = provider_factories();
    let schemas = get_schemas();

    // Collect source texts for each .crn file
    let source_texts: Vec<(PathBuf, String)> = if path.is_file() {
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        vec![(path.clone(), content)]
    } else if path.is_dir() {
        let files = find_crn_files_in_dir(path)?;
        let mut texts = Vec::new();
        for file in files {
            let content = fs::read_to_string(&file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            texts.push((file, content));
        }
        texts
    } else {
        return Err(format!("Path not found: {}", path.display()));
    };

    // Collect all List<Struct> attribute names from schemas of parsed resources
    // and build a map of attr_name -> block_name for lint suggestions
    let mut all_list_struct_attrs: HashSet<String> = HashSet::new();
    let mut block_name_suggestions: HashMap<String, String> = HashMap::new();
    for resource in &parsed.resources {
        let schema_key = provider_mod::schema_key_for_resource(&factories, resource);
        if let Some(schema) = schemas.get(&schema_key) {
            all_list_struct_attrs.extend(list_struct_attr_names(schema));
            for (attr_name, attr_schema) in &schema.attributes {
                if let Some(bn) = &attr_schema.block_name {
                    block_name_suggestions.insert(attr_name.clone(), bn.clone());
                }
            }
        }
    }

    // Scan each source file for list literal usage of List<Struct> attributes
    let mut warnings: Vec<LintWarning> = Vec::new();

    for (file_path, source) in &source_texts {
        let hits = find_list_literal_attrs(source, &all_list_struct_attrs);
        for (attr_name, line) in hits {
            let suggested_name = block_name_suggestions
                .get(&attr_name)
                .map(|s| s.as_str())
                .unwrap_or(&attr_name);
            warnings.push(LintWarning {
                file: file_path.clone(),
                line,
                message: format!(
                    "Prefer block syntax for '{}'. Use `{} {{ ... }}` instead of `{} = [{{ ... }}]`.",
                    attr_name, suggested_name, attr_name
                ),
            });
        }
    }

    if warnings.is_empty() {
        println!("{}", "No lint warnings found.".green().bold());
        Ok(())
    } else {
        for w in &warnings {
            eprintln!(
                "{} {}:{}  {}",
                "warning:".yellow().bold(),
                w.file.display(),
                w.line,
                w.message
            );
        }
        Err(format!("Found {} lint warning(s).", warnings.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_file_serde_round_trip() {
        use carina_core::plan::Plan;

        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::with_provider("aws", "s3.bucket", "my-bucket")
                .with_attribute("name", Value::String("my-bucket".to_string())),
        ));
        plan.add(Effect::Delete {
            id: ResourceId::with_provider("aws", "s3.bucket", "old-bucket"),
            identifier: "old-bucket".to_string(),
            lifecycle: LifecycleConfig::default(),
        });

        let sorted_resources = vec![
            Resource::with_provider("aws", "s3.bucket", "my-bucket")
                .with_attribute("name", Value::String("my-bucket".to_string())),
        ];

        let current_states = vec![CurrentStateEntry {
            id: ResourceId::with_provider("aws", "s3.bucket", "my-bucket"),
            state: State::not_found(ResourceId::with_provider("aws", "s3.bucket", "my-bucket")),
        }];

        let plan_file = PlanFile {
            version: 1,
            carina_version: "0.1.0".to_string(),
            timestamp: "2025-01-01T00:00:00Z".to_string(),
            source_path: "example.crn".to_string(),
            state_lineage: Some("test-lineage".to_string()),
            state_serial: Some(1),
            provider_config: ProviderConfig {
                name: "aws".to_string(),
                attributes: HashMap::from([(
                    "region".to_string(),
                    Value::String("aws.Region.ap_northeast_1".to_string()),
                )]),
            },
            backend_config: Some(BackendConfig {
                backend_type: "s3".to_string(),
                attributes: HashMap::from([
                    ("bucket".to_string(), Value::String("my-state".to_string())),
                    (
                        "key".to_string(),
                        Value::String("prod/carina.state".to_string()),
                    ),
                ]),
            }),
            plan,
            sorted_resources,
            current_states,
        };

        let json = serde_json::to_string_pretty(&plan_file).unwrap();
        let deserialized: PlanFile = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.version, 1);
        assert_eq!(deserialized.carina_version, "0.1.0");
        assert_eq!(deserialized.source_path, "example.crn");
        assert_eq!(deserialized.state_lineage, Some("test-lineage".to_string()));
        assert_eq!(deserialized.state_serial, Some(1));
        assert_eq!(deserialized.provider_config.name, "aws");
        assert!(deserialized.backend_config.is_some());
        assert_eq!(deserialized.plan.effects().len(), 2);
        assert_eq!(deserialized.sorted_resources.len(), 1);
        assert_eq!(deserialized.current_states.len(), 1);
    }

    #[test]
    fn test_resolve_attr_prefixes_extracts_prefix_and_generates_name() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "bucket_name_prefix".to_string(),
            Value::String("my-app-".to_string()),
        );

        let mut resources = vec![resource];
        resolve_attr_prefixes(&mut resources).unwrap();

        // bucket_name_prefix should be removed
        assert!(!resources[0].attributes.contains_key("bucket_name_prefix"));

        // bucket_name should be generated with the prefix
        let bucket_name = match resources[0].attributes.get("bucket_name").unwrap() {
            Value::String(s) => s.clone(),
            _ => panic!("expected String"),
        };
        assert!(bucket_name.starts_with("my-app-"));
        assert_eq!(bucket_name.len(), "my-app-".len() + 8); // prefix + 8 hex chars

        // prefixes map should have the entry
        assert_eq!(
            resources[0].prefixes.get("bucket_name"),
            Some(&"my-app-".to_string())
        );
    }

    #[test]
    fn test_resolve_attr_prefixes_leaves_non_matching_prefix_alone() {
        // If base attr doesn't exist in schema, leave _prefix as-is
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "nonexistent_attr_prefix".to_string(),
            Value::String("some-value".to_string()),
        );

        let mut resources = vec![resource];
        resolve_attr_prefixes(&mut resources).unwrap();

        // nonexistent_attr_prefix should remain untouched
        assert!(
            resources[0]
                .attributes
                .contains_key("nonexistent_attr_prefix")
        );
        assert!(resources[0].prefixes.is_empty());
    }

    #[test]
    fn test_resolve_attr_prefixes_errors_when_both_prefix_and_attr_specified() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "bucket_name_prefix".to_string(),
            Value::String("my-app-".to_string()),
        );
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("my-actual-bucket".to_string()),
        );

        let mut resources = vec![resource];
        let result = resolve_attr_prefixes(&mut resources);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot specify both"));
    }

    #[test]
    fn test_resolve_attr_prefixes_errors_on_empty_prefix() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "bucket_name_prefix".to_string(),
            Value::String("".to_string()),
        );

        let mut resources = vec![resource];
        let result = resolve_attr_prefixes(&mut resources);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot be empty"));
    }

    #[test]
    fn test_resolve_names_handles_block_name_before_prefix() {
        // resolve_names should first resolve block names, then resolve attr prefixes
        let mut resource = Resource::with_provider("awscc", "ec2.ipam", "test-ipam");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "operating_region".to_string(),
            Value::Map(
                vec![(
                    "region_name".to_string(),
                    Value::String("us-east-1".to_string()),
                )]
                .into_iter()
                .collect(),
            ),
        );

        let mut resources = vec![resource];
        resolve_names(&mut resources).unwrap();

        // operating_region should be renamed to operating_regions
        assert!(resources[0].attributes.contains_key("operating_regions"));
        assert!(!resources[0].attributes.contains_key("operating_region"));
    }

    #[test]
    fn test_reconcile_prefixed_names_reuses_state_name_when_prefix_matches() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("my-app-temporary".to_string()),
        );

        let mut state_file = StateFile::new();
        let mut rs = ResourceState::new("s3.bucket", "test-bucket", "awscc");
        rs.attributes.insert(
            "bucket_name".to_string(),
            serde_json::json!("my-app-existing1"),
        );
        rs.prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        state_file.upsert_resource(rs);

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &Some(state_file));

        // Should reuse the state name, not the temporary one
        assert_eq!(
            resources[0].attributes.get("bucket_name"),
            Some(&Value::String("my-app-existing1".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_generates_new_name_when_prefix_changes() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "new-prefix-".to_string());
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("new-prefix-abcd1234".to_string()),
        );

        let mut state_file = StateFile::new();
        let mut rs = ResourceState::new("s3.bucket", "test-bucket", "awscc");
        rs.attributes.insert(
            "bucket_name".to_string(),
            serde_json::json!("old-prefix-existing1"),
        );
        rs.prefixes
            .insert("bucket_name".to_string(), "old-prefix-".to_string());
        state_file.upsert_resource(rs);

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &Some(state_file));

        // Should keep the newly generated name since prefix changed
        assert_eq!(
            resources[0].attributes.get("bucket_name"),
            Some(&Value::String("new-prefix-abcd1234".to_string()))
        );
    }

    #[test]
    fn test_reconcile_prefixed_names_keeps_generated_name_when_no_state() {
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");
        resource
            .prefixes
            .insert("bucket_name".to_string(), "my-app-".to_string());
        resource.attributes.insert(
            "bucket_name".to_string(),
            Value::String("my-app-abcd1234".to_string()),
        );

        let mut resources = vec![resource];
        reconcile_prefixed_names(&mut resources, &None);

        // No state, so keep the generated name
        assert_eq!(
            resources[0].attributes.get("bucket_name"),
            Some(&Value::String("my-app-abcd1234".to_string()))
        );
    }

    #[test]
    fn test_detailed_exitcode_no_changes() {
        // An empty plan means no changes — has_changes should be false
        let plan = Plan::new();
        let has_changes = plan.mutation_count() > 0;
        assert!(!has_changes);
    }

    #[test]
    fn test_detailed_exitcode_with_changes() {
        // A plan with mutating effects means changes — has_changes should be true
        let mut plan = Plan::new();
        plan.add(Effect::Create(Resource::new("s3.bucket", "test")));
        let has_changes = plan.mutation_count() > 0;
        assert!(has_changes);
    }

    #[test]
    fn test_detailed_exitcode_read_only_no_changes() {
        // A plan with only Read effects should NOT count as changes
        let mut plan = Plan::new();
        plan.add(Effect::Read {
            resource: Resource::new("sts.caller_identity", "identity").with_read_only(true),
        });
        let has_changes = plan.mutation_count() > 0;
        assert!(!has_changes);
    }

    fn make_awscc_provider(region_dsl: &str) -> ProviderConfig {
        let mut attrs = HashMap::new();
        attrs.insert("region".to_string(), Value::String(region_dsl.to_string()));
        ProviderConfig {
            name: "awscc".to_string(),
            attributes: attrs,
        }
    }

    #[test]
    fn test_anonymous_id_different_regions_produce_different_identifiers() {
        // Two anonymous ec2_vpc resources with same cidr_block but different provider regions
        let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
        r1.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r1.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
        r2.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r2.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        // Use two different provider configs with different regions
        // Resources get identity from their provider, not from resource attributes
        let providers_east = vec![make_awscc_provider("awscc.Region.us_east_1")];
        let providers_west = vec![make_awscc_provider("awscc.Region.us_west_2")];

        let mut resources_east = vec![r1];
        compute_anonymous_identifiers(&mut resources_east, &providers_east).unwrap();

        let mut resources_west = vec![r2];
        compute_anonymous_identifiers(&mut resources_west, &providers_west).unwrap();

        // Both should have identifiers assigned
        assert!(!resources_east[0].id.name.is_empty());
        assert!(!resources_west[0].id.name.is_empty());
        // They must be different because providers have different regions
        assert_ne!(resources_east[0].id.name, resources_west[0].id.name);
    }

    #[test]
    fn test_anonymous_id_same_region_same_create_only_collides() {
        // Two anonymous ec2_vpc resources with same cidr_block and same provider region → collision
        let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
        r1.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r1.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
        r2.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r2.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
        let mut resources = vec![r1, r2];
        let result = compute_anonymous_identifiers(&mut resources, &providers);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("collision"));
    }

    #[test]
    fn test_anonymous_id_different_create_only_same_region_no_collision() {
        // Two anonymous ec2_vpc resources with different cidr_block in same provider region → no collision
        let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "");
        r1.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r1.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let mut r2 = Resource::with_provider("awscc", "ec2.vpc", "");
        r2.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r2.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.1.0.0/16".to_string()),
        );

        let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
        let mut resources = vec![r1, r2];
        compute_anonymous_identifiers(&mut resources, &providers).unwrap();

        assert!(!resources[0].id.name.is_empty());
        assert!(!resources[1].id.name.is_empty());
        assert_ne!(resources[0].id.name, resources[1].id.name);
    }

    #[test]
    fn test_anonymous_id_named_resources_are_skipped() {
        // Named resources should not be processed by compute_anonymous_identifiers
        let mut r1 = Resource::with_provider("awscc", "ec2.vpc", "my_vpc");
        r1.attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        r1.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let providers = vec![make_awscc_provider("awscc.Region.us_east_1")];
        let mut resources = vec![r1];
        compute_anonymous_identifiers(&mut resources, &providers).unwrap();

        // Name should remain unchanged
        assert_eq!(resources[0].id.name, "my_vpc");
    }

    #[test]
    fn test_find_state_bucket_resource_matching_type() {
        let parsed = ParsedFile {
            providers: vec![],
            backend: None,
            resources: vec![
                Resource::with_provider("aws", "s3.bucket", "my-bucket")
                    .with_attribute("name", Value::String("my-bucket".to_string())),
            ],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            inputs: vec![],
            outputs: vec![],
        };

        // Matching resource type
        assert!(
            parsed
                .find_resource_by_name("s3.bucket", "my-bucket")
                .is_some()
        );

        // Non-matching resource type
        assert!(
            parsed
                .find_resource_by_name("gcs.bucket", "my-bucket")
                .is_none()
        );

        // Non-matching bucket name
        assert!(
            parsed
                .find_resource_by_name("s3.bucket", "other-bucket")
                .is_none()
        );
    }

    #[test]
    fn validate_data_source_without_read_keyword_errors() {
        let resource = Resource::with_provider("aws", "sts.caller_identity", "identity")
            .with_attribute("_provider", Value::String("aws".to_string()));
        // read_only defaults to false, simulating missing `read` keyword
        let result = validate_resources(&[resource]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("data source"),
            "Error should mention 'data source': {}",
            err
        );
        assert!(
            err.contains("read"),
            "Error should mention 'read' keyword: {}",
            err
        );
    }

    #[test]
    fn validate_data_source_with_read_keyword_passes() {
        let resource = Resource::with_provider("aws", "sts.caller_identity", "identity")
            .with_attribute("_provider", Value::String("aws".to_string()))
            .with_read_only(true);
        let result = validate_resources(&[resource]);
        assert!(
            result.is_ok(),
            "Data source with read keyword should pass: {:?}",
            result
        );
    }

    #[test]
    fn validate_regular_resource_without_read_keyword_passes() {
        let resource = Resource::with_provider("aws", "s3.bucket", "my-bucket")
            .with_attribute("_provider", Value::String("aws".to_string()))
            .with_attribute("name", Value::String("my-bucket".to_string()))
            .with_attribute("region", Value::String("ap-northeast-1".to_string()));
        let result = validate_resources(&[resource]);
        assert!(
            result.is_ok(),
            "Regular resource without read should pass: {:?}",
            result
        );
    }

    #[test]
    fn destroy_plan_excludes_data_sources() {
        // Simulate the destroy filtering logic: data sources (read_only=true)
        // should be excluded from the destroy candidate list.
        let managed = Resource::with_provider("awscc", "ec2.vpc", "vpc");
        let data_source = Resource::with_provider("awscc", "sts.caller_identity", "identity")
            .with_read_only(true);

        let destroy_order = vec![managed, data_source];

        // Build current_states only for managed resources (data sources are skipped)
        let mut current_states: HashMap<ResourceId, State> = HashMap::new();
        for resource in &destroy_order {
            if resource.read_only {
                continue;
            }
            current_states.insert(
                resource.id.clone(),
                State::existing(resource.id.clone(), HashMap::new()),
            );
        }

        // Apply the same filtering logic as run_destroy()
        let resources_to_destroy: Vec<&Resource> = destroy_order
            .iter()
            .filter(|r| {
                if r.read_only {
                    return false;
                }
                if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
                    return false;
                }
                true
            })
            .collect();

        assert_eq!(resources_to_destroy.len(), 1);
        assert_eq!(resources_to_destroy[0].id.resource_type, "ec2.vpc");

        // Verify data source is NOT in the destroy list
        assert!(
            !resources_to_destroy.iter().any(|r| r.read_only),
            "Data sources should not appear in destroy plan"
        );
    }
}
