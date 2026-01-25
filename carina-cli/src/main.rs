use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::differ::create_plan;
use carina_core::effect::Effect;
use carina_core::formatter::{self, FormatConfig};
use carina_core::module_resolver;
use carina_core::parser::{self, ParsedFile};
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult, ResourceType};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_provider_aws::schemas;
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
        /// Path to .crn file
        #[arg(default_value = "main.crn")]
        file: PathBuf,
    },
    /// Show execution plan without applying changes
    Plan {
        /// Path to .crn file
        #[arg(default_value = "main.crn")]
        file: PathBuf,
    },
    /// Apply changes to reach the desired state
    Apply {
        /// Path to .crn file
        #[arg(default_value = "main.crn")]
        file: PathBuf,
    },
    /// Destroy all resources defined in the configuration file
    Destroy {
        /// Path to .crn file
        #[arg(default_value = "main.crn")]
        file: PathBuf,

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
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Show module structure and dependencies
    Info {
        /// Path to module .crn file
        file: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Validate { file } => run_validate(&file),
        Commands::Plan { file } => run_plan(&file).await,
        Commands::Apply { file } => run_apply(&file).await,
        Commands::Destroy { file, auto_approve } => run_destroy(&file, auto_approve).await,
        Commands::Fmt {
            path,
            check,
            diff,
            recursive,
        } => run_fmt(&path, check, diff, recursive),
        Commands::Module { command } => run_module_command(command),
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
    all_schemas
}

fn validate_resources(resources: &[Resource]) -> Result<(), String> {
    let schemas = get_schemas();
    let mut all_errors = Vec::new();

    for resource in resources {
        if let Some(schema) = schemas.get(&resource.id.resource_type)
            && let Err(errors) = schema.validate(&resource.attributes)
        {
            for error in errors {
                all_errors.push(format!(
                    "{}.{}: {}",
                    resource.id.resource_type, resource.id.name, error
                ));
            }
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

fn run_validate(file: &PathBuf) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let mut parsed =
        parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    // Resolve module imports and expand module calls
    let base_dir = file.parent().unwrap_or(Path::new("."));
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    println!("{}", "Validating...".cyan());

    validate_resources(&parsed.resources)?;

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

async fn run_plan(file: &PathBuf) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let mut parsed =
        parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    // Resolve module imports and expand module calls
    let base_dir = file.parent().unwrap_or(Path::new("."));
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    validate_resources(&parsed.resources)?;

    let plan = create_plan_from_parsed(&parsed).await?;
    print_plan(&plan);
    Ok(())
}

async fn run_apply(file: &PathBuf) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let mut parsed =
        parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    // Resolve module imports and expand module calls
    let base_dir = file.parent().unwrap_or(Path::new("."));
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    validate_resources(&parsed.resources)?;

    // Sort resources by dependencies
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Get AWS provider for route-specific reads
    let region = get_aws_region(&parsed);
    let aws_provider = AwsProvider::new(&region).await;

    // First pass: read states for non-route resources
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        if resource.id.resource_type != "route" {
            let state = provider
                .read(&resource.id)
                .await
                .map_err(|e| format!("Failed to read state: {}", e))?;
            current_states.insert(resource.id.clone(), state);
        }
    }

    // Build initial binding map for route reference resolution
    let mut route_binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &sorted_resources {
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
            route_binding_map.insert(binding_name.clone(), attrs);
        }
    }

    // Second pass: read states for route resources (need resolved route_table_id)
    for resource in &sorted_resources {
        if resource.id.resource_type == "route" {
            // Resolve route_table_id and destination_cidr_block
            let route_table_id = match resource.attributes.get("route_table_id") {
                Some(value) => {
                    let resolved = resolve_ref_value(value, &route_binding_map);
                    match resolved {
                        Value::String(s) => s,
                        _ => continue,
                    }
                }
                None => continue,
            };

            let destination_cidr = match resource.attributes.get("destination_cidr_block") {
                Some(Value::String(s)) => s.clone(),
                _ => continue,
            };

            let state = aws_provider
                .read_ec2_route_by_key(&resource.id.name, &route_table_id, &destination_cidr)
                .await
                .map_err(|e| format!("Failed to read route state: {}", e))?;
            current_states.insert(resource.id.clone(), state);
        }
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
        return Ok(());
    }

    print_plan(&plan);
    println!();

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    let mut success_count = 0;
    let mut failure_count = 0;

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

                match provider.update(id, from, &resolved_to).await {
                    Ok(state) => {
                        println!("  {} {}", "✓".green(), format_effect(effect));
                        success_count += 1;

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
            Effect::Delete(id) => match provider.delete(id).await {
                Ok(()) => {
                    println!("  {} {}", "✓".green(), format_effect(effect));
                    success_count += 1;
                }
                Err(e) => {
                    println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
                    failure_count += 1;
                }
            },
            Effect::Read(_) => {}
        }
    }

    println!();
    if failure_count == 0 {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", success_count)
                .green()
                .bold()
        );
    } else {
        println!(
            "{}",
            format!(
                "Apply failed. {} succeeded, {} failed.",
                success_count, failure_count
            )
            .red()
            .bold()
        );
    }

    Ok(())
}

async fn run_destroy(file: &PathBuf, auto_approve: bool) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let mut parsed =
        parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    // Resolve module imports and expand module calls
    let base_dir = file.parent().unwrap_or(Path::new("."));
    module_resolver::resolve_modules(&mut parsed, base_dir)
        .map_err(|e| format!("Module resolution error: {}", e))?;

    // Apply default region from provider
    apply_default_region(&mut parsed);

    if parsed.resources.is_empty() {
        println!("{}", "No resources defined in configuration.".yellow());
        return Ok(());
    }

    // Sort resources by dependencies (for creation order)
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Reverse the order for destruction (dependents first, then dependencies)
    let destroy_order: Vec<Resource> = sorted_resources.into_iter().rev().collect();

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;

    // Get AWS provider for route-specific reads
    let region = get_aws_region(&parsed);
    let aws_provider = AwsProvider::new(&region).await;

    // Build binding map for reference resolution (needed for route reads)
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();

    // First pass: read states for non-route resources
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &destroy_order {
        if resource.id.resource_type != "route" {
            let state = provider
                .read(&resource.id)
                .await
                .map_err(|e| format!("Failed to read state: {}", e))?;

            // Update binding map with state
            if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
                let mut attrs = resource.attributes.clone();
                if state.exists {
                    for (k, v) in &state.attributes {
                        if !attrs.contains_key(k) {
                            attrs.insert(k.clone(), v.clone());
                        }
                    }
                }
                binding_map.insert(binding_name.clone(), attrs);
            }

            current_states.insert(resource.id.clone(), state);
        }
    }

    // Second pass: read states for route resources (need resolved route_table_id)
    for resource in &destroy_order {
        if resource.id.resource_type == "route" {
            // Resolve route_table_id and destination_cidr_block
            let route_table_id = match resource.attributes.get("route_table_id") {
                Some(value) => {
                    let resolved = resolve_ref_value(value, &binding_map);
                    match resolved {
                        Value::String(s) => s,
                        _ => continue,
                    }
                }
                None => continue,
            };

            let destination_cidr = match resource.attributes.get("destination_cidr_block") {
                Some(Value::String(s)) => s.clone(),
                _ => continue,
            };

            let state = aws_provider
                .read_ec2_route_by_key(&resource.id.name, &route_table_id, &destination_cidr)
                .await
                .map_err(|e| format!("Failed to read route state: {}", e))?;
            current_states.insert(resource.id.clone(), state);
        }
    }

    // Collect resources that exist and will be destroyed
    let resources_to_destroy: Vec<&Resource> = destroy_order
        .iter()
        .filter(|r| current_states.get(&r.id).map(|s| s.exists).unwrap_or(false))
        .collect();

    if resources_to_destroy.is_empty() {
        println!("{}", "No resources to destroy.".green());
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

    println!();
    println!(
        "Plan: {} to destroy.",
        resources_to_destroy.len().to_string().red()
    );
    println!();

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
            return Ok(());
        }
        println!();
    }

    println!("{}", "Destroying resources...".red().bold());
    println!();

    let mut success_count = 0;
    let mut failure_count = 0;

    for resource in resources_to_destroy {
        let effect = Effect::Delete(resource.id.clone());
        match provider.delete(&resource.id).await {
            Ok(()) => {
                println!("  {} {}", "✓".green(), format_effect(&effect));
                success_count += 1;
            }
            Err(e) => {
                println!("  {} {} - {}", "✗".red(), format_effect(&effect), e);
                failure_count += 1;
            }
        }
    }

    println!();
    if failure_count == 0 {
        println!(
            "{}",
            format!("Destroy complete! {} resources destroyed.", success_count)
                .green()
                .bold()
        );
    } else {
        println!(
            "{}",
            format!(
                "Destroy failed. {} succeeded, {} failed.",
                success_count, failure_count
            )
            .red()
            .bold()
        );
    }

    Ok(())
}

/// Get region from provider configuration (DSL format: aws.Region.ap_northeast_1)
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

async fn create_plan_from_parsed(parsed: &ParsedFile) -> Result<Plan, String> {
    // Sort resources by dependencies first
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources);

    // Get AWS provider for route-specific reads
    let region = get_aws_region(parsed);
    let aws_provider = AwsProvider::new(&region).await;

    // First pass: read states for non-route resources
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        if resource.id.resource_type != "route" {
            let state = aws_provider
                .read(&resource.id)
                .await
                .map_err(|e| format!("Failed to read state: {}", e))?;
            current_states.insert(resource.id.clone(), state);
        }
    }

    // Build binding map for reference resolution
    let mut binding_map: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for resource in &sorted_resources {
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

    // Second pass: read states for route resources (need resolved route_table_id)
    for resource in &sorted_resources {
        if resource.id.resource_type == "route" {
            // Resolve route_table_id and destination_cidr_block
            let route_table_id = match resource.attributes.get("route_table_id") {
                Some(value) => {
                    let resolved = resolve_ref_value(value, &binding_map);
                    match resolved {
                        Value::String(s) => s,
                        _ => continue,
                    }
                }
                None => continue,
            };

            let destination_cidr = match resource.attributes.get("destination_cidr_block") {
                Some(Value::String(s)) => s.clone(),
                _ => continue,
            };

            let state = aws_provider
                .read_ec2_route_by_key(&resource.id.name, &route_table_id, &destination_cidr)
                .await
                .map_err(|e| format!("Failed to read route state: {}", e))?;
            current_states.insert(resource.id.clone(), state);
        }
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
            Effect::Delete(_) | Effect::Read(_) => (None, HashSet::new()),
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
            Effect::Read(_) => "?".normal(),
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
            Effect::Read(_) => {}
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
    println!(
        "Plan: {} to add, {} to change, {} to destroy.",
        summary.create.to_string().green(),
        summary.update.to_string().yellow(),
        summary.delete.to_string().red()
    );
}

fn format_effect(effect: &Effect) -> String {
    match effect {
        Effect::Create(r) => format!("Create {}.{}", r.id.resource_type, r.id.name),
        Effect::Update { id, .. } => format!("Update {}.{}", id.resource_type, id.name),
        Effect::Delete(id) => format!("Delete {}.{}", id.resource_type, id.name),
        Effect::Read(id) => format!("Read {}.{}", id.resource_type, id.name),
    }
}

/// Check if a string is in DSL enum format
/// Patterns:
/// - provider.TypeName.value (e.g., aws.Region.ap_northeast_1, gcp.Region.us_central1)
/// - TypeName.value (e.g., Region.ap_northeast_1)
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
        _ => false,
    }
}

/// Convert DSL enum format to display format (underscores to dashes, strip prefix)
/// Handles patterns like:
/// - provider.TypeName.value_name -> value-name (e.g., aws.Region.ap_northeast_1 -> ap-northeast-1)
/// - TypeName.value_name -> value-name (e.g., Region.ap_northeast_1 -> ap-northeast-1)
fn convert_enum_for_display(value: &str) -> String {
    let parts: Vec<&str> = value.split('.').collect();

    let raw_value = match parts.len() {
        2 => parts[1], // TypeName.value -> value
        3 => parts[2], // provider.TypeName.value -> value
        _ => return value.to_string(),
    };

    raw_value.replace('_', "-")
}

fn format_value(value: &Value) -> String {
    format_value_with_key(value, None)
}

fn format_value_with_key(value: &Value, _key: Option<&str>) -> String {
    match value {
        Value::String(s) => {
            // Convert DSL enum format (aws.Type.value or Type.value) to AWS format
            if is_dsl_enum_format(s) {
                return format!("\"{}\"", convert_enum_for_display(s));
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

    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            let states = self.load_states();
            let key = Self::resource_key(&id);

            if let Some(attrs) = states.get(&key) {
                let attributes: HashMap<String, Value> = attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), Self::json_to_value(v)))
                    .collect();
                Ok(State::existing(id, attributes))
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

            Ok(State::existing(
                resource.id.clone(),
                resource.attributes.clone(),
            ))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
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

    fn delete(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
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
