use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::differ::create_plan;
use carina_core::effect::Effect;
use carina_core::formatter::{self, FormatConfig};
use carina_core::module_resolver;
use carina_core::parser::{self, BackendConfig, ParsedFile, TypeExpr};
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult, ResourceType};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_core::schema::validate_cidr;
use carina_provider_aws::schemas;
use carina_provider_awscc::AwsccProvider;
use carina_state::{
    BackendConfig as StateBackendConfig, BackendError, LockInfo, ResourceState, StateBackend,
    StateFile, create_backend, create_local_backend,
};
use std::collections::HashSet;

use carina_provider_aws::AwsProvider;

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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Validate { path } => run_validate(&path),
        Commands::Plan { path } => run_plan(&path).await,
        Commands::Apply { path, auto_approve } => run_apply(&path, auto_approve).await,
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

fn get_schemas() -> HashMap<String, ResourceSchema> {
    let mut all_schemas = HashMap::new();
    for schema in schemas::all_schemas() {
        all_schemas.insert(schema.resource_type.clone(), schema);
    }
    // Add awscc schemas
    for schema in carina_provider_awscc::schemas::all_schemas() {
        all_schemas.insert(schema.resource_type.clone(), schema);
    }
    all_schemas
}

fn validate_resources(resources: &[Resource]) -> Result<(), String> {
    let schemas = get_schemas();
    let mut all_errors = Vec::new();

    for resource in resources {
        // Construct schema key based on provider
        // For aws provider, use just the resource_type (e.g., "vpc")
        // For other providers, include the provider prefix (e.g., "awscc.ec2_vpc")
        let schema_key = match resource.attributes.get("_provider") {
            Some(Value::String(provider)) if provider != "aws" => {
                format!("{}.{}", provider, resource.id.resource_type)
            }
            _ => resource.id.resource_type.clone(),
        };

        match schemas.get(&schema_key) {
            Some(schema) => {
                if let Err(errors) = schema.validate(&resource.attributes) {
                    for error in errors {
                        all_errors.push(format!(
                            "{}.{}: {}",
                            resource.id.resource_type, resource.id.name, error
                        ));
                    }
                }
            }
            None => {
                all_errors.push(format!("Unknown resource type: {}", schema_key));
            }
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Validate that resource references have compatible types.
/// For example, if `ipv4_ipam_pool_id` expects `IpamPoolId` type,
/// a reference like `vpc.vpc_id` (which is `AwsResourceId`) should be an error.
fn validate_resource_ref_types(resources: &[Resource]) -> Result<(), String> {
    let schemas = get_schemas();
    let mut all_errors = Vec::new();

    // Build binding_name -> resource map
    let mut binding_map: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            binding_map.insert(binding_name.clone(), resource);
        }
    }

    for resource in resources {
        let schema_key = match resource.attributes.get("_provider") {
            Some(Value::String(provider)) if provider != "aws" => {
                format!("{}.{}", provider, resource.id.resource_type)
            }
            _ => resource.id.resource_type.clone(),
        };

        let Some(schema) = schemas.get(&schema_key) else {
            continue;
        };

        for (attr_name, attr_value) in &resource.attributes {
            if attr_name.starts_with('_') {
                continue;
            }

            let (ref_binding, ref_attr) = match attr_value {
                Value::ResourceRef(binding, attr) => (binding, attr),
                Value::TypedResourceRef {
                    binding_name,
                    attribute_name,
                    ..
                } => (binding_name, attribute_name),
                _ => continue,
            };

            // Get the expected type for this attribute
            let Some(attr_schema) = schema.attributes.get(attr_name) else {
                continue;
            };
            let expected_type_name = attr_schema.attr_type.type_name();

            // Look up the referenced binding's schema to get the type of the referenced attribute
            let Some(ref_resource) = binding_map.get(ref_binding.as_str()) else {
                continue; // Unknown binding, skip
            };
            let ref_schema_key_str: String = match ref_resource.attributes.get("_provider") {
                Some(Value::String(provider)) if provider != "aws" => {
                    format!("{}.{}", provider, ref_resource.id.resource_type)
                }
                _ => ref_resource.id.resource_type.clone(),
            };
            let Some(ref_schema) = schemas.get(&ref_schema_key_str) else {
                continue;
            };
            let Some(ref_attr_schema) = ref_schema.attributes.get(ref_attr.as_str()) else {
                continue; // Unknown attribute on referenced resource, skip
            };
            let ref_type_name = ref_attr_schema.attr_type.type_name();

            // Type compatibility check:
            // - Same type -> OK
            // - Either is "String" -> OK (String is the base type for Custom types)
            // - Different Custom types -> Error
            if expected_type_name == ref_type_name {
                continue;
            }
            if expected_type_name == "String" || ref_type_name == "String" {
                continue;
            }

            all_errors.push(format!(
                "{}.{}: type mismatch for '{}': expected {}, got {} (from {}.{})",
                resource.id.resource_type,
                resource.id.name,
                attr_name,
                expected_type_name,
                ref_type_name,
                ref_binding,
                ref_attr,
            ));
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

fn run_module_command(command: ModuleCommands) -> Result<(), String> {
    match command {
        ModuleCommands::Info { file } => run_module_info(&file),
    }
}

fn run_module_info(path: &PathBuf) -> Result<(), String> {
    let parsed = if path.is_dir() {
        // Read all .crn files in the directory and merge them
        load_module_from_directory(path)?
    } else {
        module_resolver::get_parsed_file(path).map_err(|e| format!("Failed to load file: {}", e))?
    };

    // Derive module name from directory structure
    // For directory-based modules like modules/web_tier/, use the directory name
    // For file-based modules like modules/web_tier.crn, use the file stem
    let module_name = derive_module_name(path);

    // Build and display the file signature (module or root config)
    let signature =
        carina_core::module::FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    println!("{}", signature.display());

    Ok(())
}

/// Load a module from a directory by reading all .crn files
fn load_module_from_directory(dir: &PathBuf) -> Result<ParsedFile, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut merged = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: std::collections::HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        inputs: vec![],
        outputs: vec![],
        backend: None,
    };

    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        if path.extension().is_some_and(|ext| ext == "crn") {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            let parsed = parser::parse(&content)
                .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

            // Merge parsed content
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.inputs.extend(parsed.inputs);
            merged.outputs.extend(parsed.outputs);
        }
    }

    Ok(merged)
}

/// Derive the module name from a file or directory path
/// Examples:
/// - modules/web_tier/ -> web_tier (directory)
/// - modules/web_tier/main.crn -> web_tier (directory-based)
/// - modules/web_tier.crn -> web_tier (file-based)
/// - web_tier.crn -> web_tier
fn derive_module_name(path: &Path) -> String {
    // If it's a directory, use the directory name
    if path.is_dir() {
        return path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
    }

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // If file is named main.crn, use the parent directory name
    if file_stem == "main"
        && let Some(parent) = path.parent()
        && let Some(parent_name) = parent.file_name()
        && let Some(name) = parent_name.to_str()
    {
        return name.to_string();
    }

    // Otherwise use the file stem
    file_stem.to_string()
}

/// Validate provider region attribute
fn validate_provider_region(parsed: &ParsedFile) -> Result<(), String> {
    // Use the same region type for both aws and awscc providers
    let region_type = carina_provider_aws::schemas::types::aws_region();

    for provider in &parsed.providers {
        if (provider.name == "aws" || provider.name == "awscc")
            && let Some(region_value) = provider.attributes.get("region")
            && let Err(e) = region_type.validate(region_value)
        {
            return Err(format!("provider {}: {}", provider.name, e));
        }
    }
    Ok(())
}

/// Validate module call arguments against module input types
fn validate_module_calls(parsed: &ParsedFile, base_dir: &Path) -> Result<(), String> {
    let mut errors = Vec::new();

    // Build a map of imported modules: alias -> inputs
    let mut imported_modules = HashMap::new();
    for import in &parsed.imports {
        let module_path = base_dir.join(&import.path);
        if let Some(module_parsed) = load_module(&module_path) {
            imported_modules.insert(import.alias.clone(), module_parsed.inputs);
        }
    }

    // Validate each module call
    for call in &parsed.module_calls {
        if let Some(module_inputs) = imported_modules.get(&call.module_name) {
            for (arg_name, arg_value) in &call.arguments {
                if let Some(input) = module_inputs.iter().find(|i| &i.name == arg_name)
                    && let Some(error) = validate_module_arg_type(&input.type_expr, arg_value)
                {
                    errors.push(format!(
                        "module {} argument '{}': {}",
                        call.module_name, arg_name, error
                    ));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// Load a module from a file or directory
fn load_module(path: &Path) -> Option<ParsedFile> {
    if path.is_dir() {
        let main_path = path.join("main.crn");
        if main_path.exists() {
            let content = fs::read_to_string(&main_path).ok()?;
            parser::parse(&content).ok()
        } else {
            load_directory_module(path)
        }
    } else {
        let content = fs::read_to_string(path).ok()?;
        parser::parse(&content).ok()
    }
}

/// Load all .crn files from a directory and merge them
fn load_directory_module(dir_path: &Path) -> Option<ParsedFile> {
    let entries = fs::read_dir(dir_path).ok()?;
    let mut merged = ParsedFile {
        providers: vec![],
        resources: vec![],
        variables: HashMap::new(),
        imports: vec![],
        module_calls: vec![],
        inputs: vec![],
        outputs: vec![],
        backend: None,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn")
            && let Ok(content) = fs::read_to_string(&path)
            && let Ok(parsed) = parser::parse(&content)
        {
            merged.providers.extend(parsed.providers);
            merged.resources.extend(parsed.resources);
            merged.variables.extend(parsed.variables);
            merged.imports.extend(parsed.imports);
            merged.module_calls.extend(parsed.module_calls);
            merged.inputs.extend(parsed.inputs);
            merged.outputs.extend(parsed.outputs);
        }
    }

    if merged.inputs.is_empty() && merged.outputs.is_empty() {
        None
    } else {
        Some(merged)
    }
}

/// Result of loading configuration, includes the file path containing backend block
struct LoadedConfig {
    parsed: ParsedFile,
    backend_file: Option<PathBuf>,
}

/// Load configuration from a file or directory
fn load_configuration(path: &PathBuf) -> Result<LoadedConfig, String> {
    if path.is_file() {
        // Single file mode (existing behavior)
        let content = fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let parsed =
            parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;
        let backend_file = if parsed.backend.is_some() {
            Some(path.clone())
        } else {
            None
        };
        Ok(LoadedConfig {
            parsed,
            backend_file,
        })
    } else if path.is_dir() {
        // Directory mode
        let files = find_crn_files_in_dir(path)?;
        if files.is_empty() {
            return Err(format!("No .crn files found in {}", path.display()));
        }

        let mut merged = ParsedFile {
            providers: vec![],
            resources: vec![],
            variables: HashMap::new(),
            imports: vec![],
            module_calls: vec![],
            inputs: vec![],
            outputs: vec![],
            backend: None,
        };
        let mut parse_errors = Vec::new();
        let mut backend_file: Option<PathBuf> = None;

        for file in &files {
            let content = fs::read_to_string(file)
                .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;
            match parser::parse_and_resolve(&content) {
                Ok(parsed) => {
                    merged.providers.extend(parsed.providers);
                    merged.resources.extend(parsed.resources);
                    merged.variables.extend(parsed.variables);
                    merged.imports.extend(parsed.imports);
                    merged.module_calls.extend(parsed.module_calls);
                    merged.inputs.extend(parsed.inputs);
                    merged.outputs.extend(parsed.outputs);
                    // Merge backend (only one allowed)
                    if let Some(backend) = parsed.backend {
                        if merged.backend.is_some() {
                            parse_errors.push(format!(
                                "{}: multiple backend blocks defined",
                                file.display()
                            ));
                        } else {
                            merged.backend = Some(backend);
                            backend_file = Some(file.clone());
                        }
                    }
                }
                Err(e) => {
                    parse_errors.push(format!("{}: {}", file.display(), e));
                }
            }
        }

        if !parse_errors.is_empty() {
            return Err(parse_errors.join("\n"));
        }
        Ok(LoadedConfig {
            parsed: merged,
            backend_file,
        })
    } else {
        Err(format!("Path not found: {}", path.display()))
    }
}

/// Get base directory for module resolution
fn get_base_dir(path: &Path) -> &Path {
    if path.is_file() {
        path.parent().unwrap_or(Path::new("."))
    } else {
        path
    }
}

/// Validate a module argument value against its expected type
fn validate_module_arg_type(type_expr: &TypeExpr, value: &Value) -> Option<String> {
    match (type_expr, value) {
        (TypeExpr::Cidr, Value::String(s)) => validate_cidr(s).err(),
        (TypeExpr::List(inner), Value::List(items)) => {
            if let TypeExpr::Cidr = inner.as_ref() {
                for (i, item) in items.iter().enumerate() {
                    if let Value::String(s) = item {
                        if let Err(e) = validate_cidr(s) {
                            return Some(format!("element {}: {}", i, e));
                        }
                    } else {
                        return Some(format!("element {}: expected string", i));
                    }
                }
            }
            None
        }
        (TypeExpr::Bool, Value::String(s)) => Some(format!(
            "expected bool, got string \"{}\". Use true or false.",
            s
        )),
        (TypeExpr::Int, Value::String(s)) => Some(format!("expected int, got string \"{}\".", s)),
        _ => None,
    }
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

    // Apply default region from provider
    apply_default_region(&mut parsed);

    println!("{}", "Validating...".cyan());

    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;

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
        println!("  • {}.{}", resource.id.resource_type, resource.id.name);
    }

    Ok(())
}

async fn run_plan(path: &PathBuf) -> Result<(), String> {
    let mut parsed = load_configuration(path)?.parsed;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Validate provider region
    validate_provider_region(&parsed)?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;

    // Check for backend configuration and load state
    // Use local backend by default if no backend is configured
    let mut will_create_state_bucket = false;
    let mut state_bucket_name = String::new();
    let mut state_file: Option<StateFile> = None;

    let _backend: Box<dyn StateBackend> = if let Some(config) = parsed.backend.as_ref() {
        let state_config = convert_backend_config(config);
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
            // Check if there's a matching s3.bucket resource defined
            let bucket_name = config
                .attributes
                .get("bucket")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .ok_or("Backend bucket name not specified")?;

            let has_bucket_resource = parsed.resources.iter().any(|r| {
                r.id.resource_type == "s3.bucket"
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
        println!("{}", "Bootstrap Plan:".cyan().bold());
        println!(
            "  {} {} (state bucket with versioning enabled)",
            "+".green(),
            format!("aws.s3.bucket.{}", state_bucket_name).green()
        );
        println!(
            "  {} Resource definition will be added to .crn file",
            "→".cyan()
        );
        println!();
    }

    let plan = create_plan_from_parsed(&parsed, &state_file).await?;
    print_plan(&plan);
    Ok(())
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

    // Apply default region from provider
    apply_default_region(&mut parsed);

    validate_resources(&parsed.resources)?;
    validate_resource_ref_types(&parsed.resources)?;

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = convert_backend_config(config);
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

            // Check if there's an s3.bucket resource defined with matching name
            if let Some(bucket_resource) = find_state_bucket_resource(&parsed, &bucket_name) {
                println!("Found state bucket resource in configuration.");
                println!(
                    "Creating bucket '{}' before other resources...",
                    bucket_name.cyan()
                );

                // Create the bucket resource first
                let region = get_aws_region(&parsed);
                let aws_provider = AwsProvider::new(&region).await;

                match aws_provider.create(bucket_resource).await {
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

                    // Get region from backend config
                    let region = config
                        .attributes
                        .get("region")
                        .and_then(|v| match v {
                            Value::String(s) => Some(convert_region_value(s)),
                            _ => None,
                        })
                        .unwrap_or_else(|| "ap-northeast-1".to_string());

                    // Append resource definition to backend file
                    let target_file = backend_file.clone().unwrap_or_else(|| path.clone());

                    let resource_code = format!(
                        "\n# Auto-generated by carina (state bucket)\naws.s3.bucket {{\n    name       = \"{}\"\n    versioning = \"Enabled\"\n}}\n",
                        bucket_name
                    );

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
                    let bucket_state = ResourceState::new("s3.bucket", &bucket_name, "aws")
                        .with_attribute("name".to_string(), serde_json::json!(bucket_name))
                        .with_attribute("region".to_string(), serde_json::json!(region))
                        .with_attribute("versioning".to_string(), serde_json::json!("Enabled"))
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

    // Sort resources by dependencies
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = get_identifier_from_state(&state_file, resource);
        let state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
        current_states.insert(resource.id.clone(), state);
    }

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

    // Resolve references and create initial plan for display
    let mut resources_for_plan = sorted_resources.clone();
    resolve_refs_with_state(&mut resources_for_plan, &current_states);
    let plan = create_plan(&resources_for_plan, &current_states);

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
    let mut applied_states: HashMap<ResourceId, State> = HashMap::new();

    // Apply each effect in order, resolving references dynamically
    for effect in plan.effects() {
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
                    }
                }
            }
            Effect::Delete(id) => {
                // Get identifier from current state
                let identifier = current_states
                    .get(id)
                    .and_then(|s| s.identifier.as_deref())
                    .unwrap_or("");
                match provider.delete(id, identifier).await {
                    Ok(()) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;
                    }
                    Err(e) => {
                        println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                        failure_count += 1;
                    }
                }
            }
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
            let resource_state = resource_to_state(resource, applied_state, existing);
            state.upsert_resource(resource_state);
        } else if let Some(current_state) = current_states.get(&resource.id)
            && current_state.exists
        {
            let resource_state = resource_to_state(resource, current_state, existing);
            state.upsert_resource(resource_state);
        }
    }

    // Remove deleted resources from state
    for effect in plan.effects() {
        if let Effect::Delete(id) = effect {
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
    if failure_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        Err(format!(
            "Apply failed. {} succeeded, {} failed.",
            success_count, failure_count
        ))
    }
}

async fn run_destroy(path: &PathBuf, auto_approve: bool) -> Result<(), String> {
    let mut parsed = load_configuration(path)?.parsed;

    // Resolve module imports and expand module calls
    let base_dir = get_base_dir(path);
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Validate provider region
    validate_provider_region(&parsed)?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    if parsed.resources.is_empty() {
        println!("{}", "No resources defined in configuration.".yellow());
        return Ok(());
    }

    // Check for backend configuration - use local backend by default
    let backend_config = parsed.backend.as_ref();
    let backend: Box<dyn StateBackend> = if let Some(config) = backend_config {
        let state_config = convert_backend_config(config);
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

    // Sort resources by dependencies (for creation order)
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Reverse the order for destruction (dependents first, then dependencies)
    let destroy_order: Vec<Resource> = sorted_resources.into_iter().rev().collect();

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Read states for all resources using identifier from state
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &destroy_order {
        let identifier = get_identifier_from_state(&state_file, resource);
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
            if !current_states.get(&r.id).map(|s| s.exists).unwrap_or(false) {
                return false;
            }

            // Check if this is the protected state bucket
            if r.id.resource_type == "s3.bucket"
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
        println!(
            "  {} {}.{}",
            "-".red().bold(),
            resource.id.resource_type,
            resource.id.name
        );
    }

    // Show protected resources
    for resource in &protected_resources {
        println!(
            "  {} {}.{} {}",
            "⚠".yellow().bold(),
            resource.id.resource_type,
            resource.id.name,
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

    let mut success_count = 0;
    let mut failure_count = 0;
    let mut destroyed_ids: Vec<ResourceId> = Vec::new();

    for resource in &resources_to_destroy {
        let effect = Effect::Delete(resource.id.clone());

        // Get identifier from current state
        let identifier = current_states
            .get(&resource.id)
            .and_then(|s| s.identifier.as_deref())
            .unwrap_or("");

        let delete_result = provider.delete(&resource.id, identifier).await;

        match delete_result {
            Ok(()) => {
                println!("  {} {}", "✓".green(), format_effect(&effect));
                success_count += 1;
                destroyed_ids.push(resource.id.clone());
            }
            Err(e) => {
                println!("  {} {} - {}", "✗".red(), format_effect(&effect), e);
                failure_count += 1;
            }
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
    if failure_count == 0 {
        println!(
            "{}",
            format!("Destroy complete! {} resources destroyed.", success_count)
                .green()
                .bold()
        );
        Ok(())
    } else {
        Err(format!(
            "Destroy failed. {} succeeded, {} failed.",
            success_count, failure_count
        ))
    }
}

/// Get identifier from state file for a resource
fn get_identifier_from_state(
    state_file: &Option<StateFile>,
    resource: &Resource,
) -> Option<String> {
    if let Some(state) = state_file
        && let Some(resource_state) =
            state.find_resource(&resource.id.resource_type, &resource.id.name)
    {
        return resource_state.identifier.clone();
    }
    None
}

fn get_aws_region_dsl(parsed: &ParsedFile) -> Option<String> {
    for provider in &parsed.providers {
        if provider.name == "aws"
            && let Some(Value::String(region)) = provider.attributes.get("region")
        {
            return Some(region.clone());
        }
    }
    None
}

/// Get region from provider configuration (AWS format: ap-northeast-1)
fn get_aws_region(parsed: &ParsedFile) -> String {
    if let Some(region) = get_aws_region_dsl(parsed) {
        // Convert from aws.Region.ap_northeast_1 format to ap-northeast-1 format
        if region.starts_with("aws.Region.") {
            return region
                .strip_prefix("aws.Region.")
                .unwrap_or(&region)
                .replace('_', "-");
        }
        return region;
    }
    // Default region
    "ap-northeast-1".to_string()
}

/// Get region from awscc provider configuration (AWS format: ap-northeast-1)
fn get_awscc_region(parsed: &ParsedFile) -> String {
    for provider in &parsed.providers {
        if provider.name == "awscc"
            && let Some(Value::String(region)) = provider.attributes.get("region")
        {
            // Convert from aws.Region.ap_northeast_1 format to ap-northeast-1 format
            if region.starts_with("aws.Region.") {
                return region
                    .strip_prefix("aws.Region.")
                    .unwrap_or(region)
                    .replace('_', "-");
            }
            return region.clone();
        }
    }
    // Default region
    "ap-northeast-1".to_string()
}

/// Apply default region from provider to resources that don't have a region specified
fn apply_default_region(parsed: &mut ParsedFile) {
    if let Some(default_region) = get_aws_region_dsl(parsed) {
        for resource in &mut parsed.resources {
            if !resource.attributes.contains_key("region") {
                resource
                    .attributes
                    .insert("region".to_string(), Value::String(default_region.clone()));
            }
        }
    }
}

/// Determine and return the appropriate Provider
async fn get_provider(parsed: &ParsedFile) -> Box<dyn Provider> {
    // Use AwsProvider if AWS provider is configured
    for provider in &parsed.providers {
        if provider.name == "aws" {
            let region = get_aws_region(parsed);
            println!(
                "{}",
                format!("Using AWS provider (region: {})", region).cyan()
            );
            return Box::new(AwsProvider::new(&region).await);
        }
        if provider.name == "awscc" {
            let region = get_awscc_region(parsed);
            println!(
                "{}",
                format!("Using AWS Cloud Control provider (region: {})", region).cyan()
            );
            return Box::new(AwsccProvider::new(&region).await);
        }
    }

    // Use file-based mock for other cases
    println!("{}", "Using file-based mock provider".cyan());
    Box::new(FileProvider::new())
}

/// Resolve ResourceRef values using current AWS state
fn resolve_refs_with_state(
    resources: &mut [Resource],
    current_states: &HashMap<ResourceId, State>,
) {
    // Build a map of binding_name -> attributes (merged from DSL and AWS state)
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

    for resource in resources.iter() {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            let mut attrs = resource.attributes.clone();

            // Merge AWS state attributes (like `id`) if available
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

    // Resolve ResourceRef values in all resources
    for resource in resources.iter_mut() {
        let mut resolved_attrs = HashMap::new();
        for (key, value) in &resource.attributes {
            resolved_attrs.insert(key.clone(), resolve_ref_value(value, &binding_map));
        }
        resource.attributes = resolved_attrs;
    }
}

fn resolve_ref_value(
    value: &Value,
    binding_map: &HashMap<String, HashMap<String, Value>>,
) -> Value {
    match value {
        Value::ResourceRef(binding_name, attr_name) => {
            if let Some(attrs) = binding_map.get(binding_name)
                && let Some(attr_value) = attrs.get(attr_name)
            {
                // Recursively resolve
                return resolve_ref_value(attr_value, binding_map);
            }
            // Keep as-is if not found
            value.clone()
        }
        Value::TypedResourceRef {
            binding_name,
            attribute_name,
            ..
        } => {
            if let Some(attrs) = binding_map.get(binding_name)
                && let Some(attr_value) = attrs.get(attribute_name)
            {
                // Recursively resolve
                return resolve_ref_value(attr_value, binding_map);
            }
            // Keep as-is if not found
            value.clone()
        }
        Value::List(items) => Value::List(
            items
                .iter()
                .map(|v| resolve_ref_value(v, binding_map))
                .collect(),
        ),
        Value::Map(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (k.clone(), resolve_ref_value(v, binding_map)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

/// Extract binding names that a resource depends on
fn get_resource_dependencies(resource: &Resource) -> HashSet<String> {
    let mut deps = HashSet::new();
    for value in resource.attributes.values() {
        collect_dependencies(value, &mut deps);
    }
    deps
}

fn collect_dependencies(value: &Value, deps: &mut HashSet<String>) {
    match value {
        Value::ResourceRef(binding_name, _) => {
            deps.insert(binding_name.clone());
        }
        Value::TypedResourceRef { binding_name, .. } => {
            deps.insert(binding_name.clone());
        }
        Value::List(items) => {
            for item in items {
                collect_dependencies(item, deps);
            }
        }
        Value::Map(map) => {
            for v in map.values() {
                collect_dependencies(v, deps);
            }
        }
        _ => {}
    }
}

/// Sort resources topologically based on dependencies
fn sort_resources_by_dependencies(resources: &[Resource]) -> Vec<Resource> {
    // Build binding name to resource mapping
    let mut binding_to_resource: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            binding_to_resource.insert(binding_name.clone(), resource);
        }
    }

    // Build dependency graph
    let mut sorted = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();

    fn visit<'a>(
        resource: &'a Resource,
        binding_to_resource: &HashMap<String, &'a Resource>,
        visited: &mut HashSet<String>,
        visiting: &mut HashSet<String>,
        sorted: &mut Vec<Resource>,
    ) {
        let binding_name = resource
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));

        if visited.contains(&binding_name) {
            return;
        }
        if visiting.contains(&binding_name) {
            // Circular dependency - just continue
            return;
        }

        visiting.insert(binding_name.clone());

        // Visit dependencies first
        let deps = get_resource_dependencies(resource);
        for dep in deps {
            if let Some(dep_resource) = binding_to_resource.get(&dep) {
                visit(dep_resource, binding_to_resource, visited, visiting, sorted);
            }
        }

        visiting.remove(&binding_name);
        visited.insert(binding_name);
        sorted.push(resource.clone());
    }

    for resource in resources {
        visit(
            resource,
            &binding_to_resource,
            &mut visited,
            &mut visiting,
            &mut sorted,
        );
    }

    sorted
}

async fn create_plan_from_parsed(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
) -> Result<Plan, String> {
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(parsed).await;

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = get_identifier_from_state(state_file, resource);
        let state = provider
            .read(&resource.id, identifier.as_deref())
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
        current_states.insert(resource.id.clone(), state);
    }

    // Resolve ResourceRef values using AWS state
    let mut resources = sorted_resources;
    resolve_refs_with_state(&mut resources, &current_states);

    Ok(create_plan(&resources, &current_states))
}

fn print_plan(plan: &Plan) {
    if plan.is_empty() {
        println!("{}", "No changes. Infrastructure is up-to-date.".green());
        return;
    }

    // Build dependency graph from effects
    let mut binding_to_effect: HashMap<String, usize> = HashMap::new();
    let mut effect_deps: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut effect_bindings: HashMap<usize, String> = HashMap::new();

    for (idx, effect) in plan.effects().iter().enumerate() {
        let (resource, deps) = match effect {
            Effect::Create(r) => (Some(r), get_resource_dependencies(r)),
            Effect::Update { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Read { resource } => (Some(resource), get_resource_dependencies(resource)),
            Effect::Delete(_) => (None, HashSet::new()),
        };

        if let Some(r) = resource {
            let binding = r
                .attributes
                .get("_binding")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| format!("{}.{}", r.id.resource_type, r.id.name));
            binding_to_effect.insert(binding.clone(), idx);
            effect_bindings.insert(idx, binding);
        }
        effect_deps.insert(idx, deps);
    }

    // Build reverse dependency map (who depends on this resource)
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        dependents.insert(idx, Vec::new());
    }

    for (idx, deps) in &effect_deps {
        for dep in deps {
            if let Some(&dep_idx) = binding_to_effect.get(dep) {
                dependents.get_mut(&dep_idx).unwrap().push(*idx);
            }
        }
    }

    // Find root resources (no dependencies within the plan)
    let mut roots: Vec<usize> = Vec::new();
    for (idx, deps) in &effect_deps {
        let has_dep_in_plan = deps.iter().any(|d| binding_to_effect.contains_key(d));
        if !has_dep_in_plan {
            roots.push(*idx);
        }
    }
    roots.sort();

    println!("{}", "Execution Plan:".cyan().bold());
    println!();

    // Track printed effects to avoid duplicates
    let mut printed: HashSet<usize> = HashSet::new();

    fn print_effect_tree(
        idx: usize,
        plan: &Plan,
        dependents: &HashMap<usize, Vec<usize>>,
        printed: &mut HashSet<usize>,
        indent: usize,
        is_last: bool,
        prefix: &str,
    ) {
        if printed.contains(&idx) {
            return;
        }
        printed.insert(idx);

        let effect = &plan.effects()[idx];
        let colored_symbol = match effect {
            Effect::Create(_) => "+".green().bold(),
            Effect::Update { .. } => "~".yellow().bold(),
            Effect::Delete(_) => "-".red().bold(),
            Effect::Read { .. } => "<=".cyan().bold(),
        };

        // Build the tree connector (shown before child resources)
        let connector = if indent == 0 {
            "".to_string()
        } else if is_last {
            format!("{}└─ ", prefix)
        } else {
            format!("{}├─ ", prefix)
        };

        // Base indentation for this resource
        let base_indent = "  ";
        // Attribute indentation (4 spaces from resource line)
        let attr_base = "    ";

        match effect {
            Effect::Create(r) => {
                println!(
                    "{}{}{} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    r.id.resource_type.cyan().bold()
                );
                // Attribute prefix aligns with the resource content
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                let mut keys: Vec<_> = r
                    .attributes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .collect();
                keys.sort_by(|a, b| match (a.as_str(), b.as_str()) {
                    ("name", _) => std::cmp::Ordering::Less,
                    (_, "name") => std::cmp::Ordering::Greater,
                    _ => a.cmp(b),
                });
                for key in keys {
                    let value = &r.attributes[key];
                    if key == "name" {
                        println!(
                            "{}{}: {}",
                            attr_prefix,
                            key.bold(),
                            format_value_with_key(value, Some(key)).white().bold()
                        );
                    } else {
                        println!(
                            "{}{}: {}",
                            attr_prefix,
                            key,
                            format_value_with_key(value, Some(key)).green()
                        );
                    }
                }
            }
            Effect::Update { id, from, to, .. } => {
                println!(
                    "{}{}{} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.resource_type.cyan().bold()
                );
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                let mut keys: Vec<_> = to
                    .attributes
                    .keys()
                    .filter(|k| !k.starts_with('_'))
                    .collect();
                keys.sort_by(|a, b| match (a.as_str(), b.as_str()) {
                    ("name", _) => std::cmp::Ordering::Less,
                    (_, "name") => std::cmp::Ordering::Greater,
                    _ => a.cmp(b),
                });
                for key in keys {
                    let new_value = &to.attributes[key];
                    let old_value = from.attributes.get(key);
                    if old_value != Some(new_value) {
                        let old_str = old_value
                            .map(|v| format_value_with_key(v, Some(key)))
                            .unwrap_or_else(|| "(none)".to_string());
                        if key == "name" {
                            println!(
                                "{}{}: {} → {}",
                                attr_prefix,
                                key.bold(),
                                old_str.red(),
                                format_value_with_key(new_value, Some(key)).white().bold()
                            );
                        } else {
                            println!(
                                "{}{}: {} → {}",
                                attr_prefix,
                                key,
                                old_str.red(),
                                format_value_with_key(new_value, Some(key)).green()
                            );
                        }
                    }
                }
            }
            Effect::Delete(id) => {
                println!(
                    "{}{}{} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    id.resource_type.cyan().bold()
                );
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                println!("{}{}: {}", attr_prefix, "name".bold(), id.name.red().bold());
            }
            Effect::Read { resource } => {
                println!(
                    "{}{}{} {} {}",
                    base_indent,
                    connector,
                    colored_symbol,
                    resource.id.resource_type.cyan().bold(),
                    "(data source)".dimmed()
                );
                let attr_prefix = if indent == 0 {
                    format!("{}{}", base_indent, attr_base)
                } else {
                    let continuation = if is_last {
                        format!("{}   ", prefix)
                    } else {
                        format!("{}│  ", prefix)
                    };
                    format!("{}{}   ", base_indent, continuation)
                };
                println!(
                    "{}{}: {}",
                    attr_prefix,
                    "name".bold(),
                    resource.id.name.cyan().bold()
                );
            }
        }

        // Print children (dependents)
        let children = dependents.get(&idx).cloned().unwrap_or_default();
        let unprinted_children: Vec<_> = children
            .iter()
            .filter(|c| !printed.contains(c))
            .cloned()
            .collect();

        // New prefix for children: align with attribute indentation
        let new_prefix = if indent == 0 {
            format!("{}  ", attr_base)
        } else {
            let continuation = if is_last {
                format!("{}   ", prefix)
            } else {
                format!("{}│  ", prefix)
            };
            format!("{}   ", continuation)
        };

        for (i, child_idx) in unprinted_children.iter().enumerate() {
            let child_is_last = i == unprinted_children.len() - 1;
            print_effect_tree(
                *child_idx,
                plan,
                dependents,
                printed,
                indent + 1,
                child_is_last,
                &new_prefix,
            );
        }
    }

    // Print from roots
    for (i, root_idx) in roots.iter().enumerate() {
        print_effect_tree(
            *root_idx,
            plan,
            &dependents,
            &mut printed,
            0,
            i == roots.len() - 1,
            "",
        );
    }

    // Print any remaining effects that weren't reachable from roots
    // (e.g., circular dependencies or isolated resources)
    let remaining: Vec<_> = (0..plan.effects().len())
        .filter(|idx| !printed.contains(idx))
        .collect();
    for idx in remaining {
        print_effect_tree(idx, plan, &dependents, &mut printed, 0, true, "");
    }

    println!();
    let summary = plan.summary();
    if summary.read > 0 {
        println!(
            "Plan: {} to read, {} to add, {} to change, {} to destroy.",
            summary.read.to_string().cyan(),
            summary.create.to_string().green(),
            summary.update.to_string().yellow(),
            summary.delete.to_string().red()
        );
    } else {
        println!(
            "Plan: {} to add, {} to change, {} to destroy.",
            summary.create.to_string().green(),
            summary.update.to_string().yellow(),
            summary.delete.to_string().red()
        );
    }
}

fn format_effect(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("Create {}.{}", r.id.resource_type, r.id.name),
        Effect::Update { id, .. } => format!("Update {}.{}", id.resource_type, id.name),
        Effect::Delete(id) => format!("Delete {}.{}", id.resource_type, id.name),
        Effect::Read { resource } => {
            format!("Read {}.{}", resource.id.resource_type, resource.id.name)
        }
    }
}

/// Check if a string is in DSL enum format
/// Patterns:
/// - provider.TypeName.value (e.g., aws.Region.ap_northeast_1, gcp.Region.us_central1)
/// - TypeName.value (e.g., Region.ap_northeast_1)
/// - provider.resource.TypeName.value (e.g., aws.s3.VersioningStatus.Enabled, awscc.ec2_vpc.InstanceTenancy.default)
fn is_dsl_enum_format(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();

    match parts.len() {
        // TypeName.value
        2 => parts[0].chars().next().is_some_and(|c| c.is_uppercase()),
        // provider.TypeName.value
        3 => {
            let provider = parts[0];
            let type_name = parts[1];
            // provider should be lowercase, TypeName should start with uppercase
            provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
        }
        // provider.resource.TypeName.value (e.g., aws.s3.VersioningStatus.Enabled)
        4 => {
            let provider = parts[0];
            let resource = parts[1];
            let type_name = parts[2];
            // provider and resource should be lowercase/digits, TypeName should start with uppercase
            provider.chars().all(|c| c.is_lowercase())
                && resource
                    .chars()
                    .all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
        }
        _ => false,
    }
}

fn format_value(value: &Value) -> String {
    format_value_with_key(value, None)
}

fn format_value_with_key(value: &Value, _key: Option<&str>) -> String {
    match value {
        Value::String(s) => {
            // DSL enum format (namespaced identifiers) - display without quotes
            if is_dsl_enum_format(s) {
                return s.clone();
            }
            format!("\"{}\"", s)
        }
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::List(items) => {
            let strs: Vec<_> = items.iter().map(format_value).collect();
            format!("[{}]", strs.join(", "))
        }
        Value::Map(map) => {
            let strs: Vec<_> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                .collect();
            format!("{{{}}}", strs.join(", "))
        }
        Value::ResourceRef(binding, attr) => format!("{}.{}", binding, attr),
        Value::TypedResourceRef {
            binding_name,
            attribute_name,
            ..
        } => format!("{}.{}", binding_name, attribute_name),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => format!("{}.{}", name, m),
            None => name.clone(),
        },
    }
}

// =============================================================================
// State Management Functions
// =============================================================================

/// Convert parser BackendConfig to state BackendConfig
fn convert_backend_config(config: &BackendConfig) -> StateBackendConfig {
    StateBackendConfig {
        backend_type: config.backend_type.clone(),
        attributes: config.attributes.clone(),
    }
}

/// Find the state bucket resource in the parsed file
fn find_state_bucket_resource<'a>(
    parsed: &'a ParsedFile,
    bucket_name: &str,
) -> Option<&'a Resource> {
    parsed.resources.iter().find(|r| {
        r.id.resource_type == "s3.bucket"
            && matches!(r.attributes.get("name"), Some(Value::String(name)) if name == bucket_name)
    })
}

/// Convert a Resource to ResourceState for the state file
fn resource_to_state(
    resource: &Resource,
    state: &State,
    existing_state: Option<&ResourceState>,
) -> ResourceState {
    let provider = resource
        .attributes
        .get("_provider")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "aws".to_string());

    let mut resource_state =
        ResourceState::new(&resource.id.resource_type, &resource.id.name, provider);

    // Copy identifier from state
    resource_state.identifier = state.identifier.clone();

    // Set attributes directly (not nested)
    for (k, v) in &state.attributes {
        resource_state
            .attributes
            .insert(k.clone(), value_to_json(v));
    }

    // Preserve protected flag from existing state
    if let Some(existing) = existing_state {
        resource_state.protected = existing.protected;
    }

    resource_state
}

/// Convert Value to serde_json::Value
fn value_to_json(value: &Value) -> serde_json::Value {
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Int(n) => serde_json::Value::Number((*n).into()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::List(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<_, _> = map
                .iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::ResourceRef(binding, attr) => {
            serde_json::Value::String(format!("${{{}.{}}}", binding, attr))
        }
        Value::TypedResourceRef {
            binding_name,
            attribute_name,
            ..
        } => serde_json::Value::String(format!("${{{}.{}}}", binding_name, attribute_name)),
        Value::UnresolvedIdent(name, member) => match member {
            Some(m) => serde_json::Value::String(format!("{}.{}", name, m)),
            None => serde_json::Value::String(name.clone()),
        },
    }
}

/// Run force-unlock command
async fn run_force_unlock(lock_id: &str, path: &PathBuf) -> Result<(), String> {
    let parsed = load_configuration(path)?.parsed;

    let backend_config = parsed
        .backend
        .as_ref()
        .ok_or("No backend configuration found. force-unlock requires a backend.")?;

    let state_config = convert_backend_config(backend_config);
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

    // Get region from backend config
    let region = backend_config
        .attributes
        .get("region")
        .and_then(|v| match v {
            Value::String(s) => Some(convert_region_value(s)),
            _ => None,
        })
        .unwrap_or_else(|| "ap-northeast-1".to_string());

    // Create AWS provider to delete the bucket
    let aws_provider = AwsProvider::new(&region).await;

    // First, try to empty the bucket (delete all objects and versions)
    println!();
    println!("{}", "Emptying bucket...".cyan());

    // Delete the bucket resource (for S3, identifier is the bucket name)
    let bucket_id = ResourceId::new("s3.bucket", bucket_name);
    match aws_provider.delete(&bucket_id, bucket_name).await {
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

/// Convert region value from DSL format to AWS format
fn convert_region_value(value: &str) -> String {
    if value.starts_with("aws.Region.") {
        value
            .strip_prefix("aws.Region.")
            .unwrap_or(value)
            .replace('_', "-")
    } else {
        value.to_string()
    }
}

// File-based mock Provider (saves state to JSON file)
struct FileProvider {
    state_file: PathBuf,
}

impl FileProvider {
    fn new() -> Self {
        Self {
            state_file: PathBuf::from(".carina/state.json"),
        }
    }

    fn load_states(&self) -> HashMap<String, HashMap<String, serde_json::Value>> {
        if let Ok(content) = fs::read_to_string(&self.state_file) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        }
    }

    fn save_states(
        &self,
        states: &HashMap<String, HashMap<String, serde_json::Value>>,
    ) -> Result<(), std::io::Error> {
        if let Some(parent) = self.state_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(states)?;
        fs::write(&self.state_file, content)
    }

    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.name)
    }

    fn value_to_json(value: &Value) -> serde_json::Value {
        match value {
            Value::String(s) => serde_json::Value::String(s.clone()),
            Value::Int(n) => serde_json::Value::Number((*n).into()),
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(Self::value_to_json).collect())
            }
            Value::Map(map) => {
                let obj: serde_json::Map<_, _> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::value_to_json(v)))
                    .collect();
                serde_json::Value::Object(obj)
            }
            // ResourceRef should be resolved before reaching here, but handle it as a string
            Value::ResourceRef(binding, attr) => {
                serde_json::Value::String(format!("${{{}.{}}}", binding, attr))
            }
            // TypedResourceRef should be resolved before reaching here, but handle it as a string
            Value::TypedResourceRef {
                binding_name,
                attribute_name,
                ..
            } => serde_json::Value::String(format!("${{{}.{}}}", binding_name, attribute_name)),
            // UnresolvedIdent should be resolved before reaching here, but handle it as a string
            Value::UnresolvedIdent(name, member) => match member {
                Some(m) => serde_json::Value::String(format!("{}.{}", name, m)),
                None => serde_json::Value::String(name.clone()),
            },
        }
    }

    fn json_to_value(json: &serde_json::Value) -> Value {
        match json {
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Number(n) => Value::Int(n.as_i64().unwrap_or(0)),
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Array(items) => {
                Value::List(items.iter().map(Self::json_to_value).collect())
            }
            serde_json::Value::Object(map) => {
                let m: HashMap<_, _> = map
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::json_to_value(v)))
                    .collect();
                Value::Map(m)
            }
            serde_json::Value::Null => Value::String("null".to_string()),
        }
    }
}

impl Provider for FileProvider {
    fn name(&self) -> &'static str {
        "file"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        vec![]
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            let states = self.load_states();
            let key = Self::resource_key(&id);

            if let Some(attrs) = states.get(&key) {
                let attributes: HashMap<String, Value> = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::json_to_value(v)))
                    .collect();
                Ok(State::existing(id, attributes).with_identifier("file-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&resource.id);

            let attrs: HashMap<String, serde_json::Value> = resource
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), Self::value_to_json(v)))
                .collect();

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(
                State::existing(resource.id.clone(), resource.attributes.clone())
                    .with_identifier("file-id"),
            )
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let to = to.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            let attrs: HashMap<String, serde_json::Value> = to
                .attributes
                .iter()
                .map(|(k, v)| (k.clone(), Self::value_to_json(v)))
                .collect();

            states.insert(key, attrs);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(State::existing(id, to.attributes.clone()))
        })
    }

    fn delete(&self, id: &ResourceId, _identifier: &str) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            let mut states = self.load_states();
            let key = Self::resource_key(&id);

            states.remove(&key);
            self.save_states(&states)
                .map_err(|e| ProviderError::new(format!("Failed to save state: {}", e)))?;

            Ok(())
        })
    }
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

fn find_crn_files_recursive(dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_crn_files_recursive(dir, &mut files)?;
    Ok(files)
}

fn collect_crn_files_recursive(dir: &PathBuf, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();

        if path.is_dir() {
            // Skip hidden directories and common non-source directories
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !name.starts_with('.') && name != "target" && name != "node_modules" {
                collect_crn_files_recursive(&path, files)?;
            }
        } else if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }

    Ok(())
}

fn find_crn_files_in_dir(dir: &PathBuf) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir)
        .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "crn") {
            files.push(path);
        }
    }
    Ok(files)
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
