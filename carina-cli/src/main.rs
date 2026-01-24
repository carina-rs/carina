use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::differ::create_plan;
use carina_core::effect::Effect;
use carina_core::formatter::{self, FormatConfig};
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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Validate { file } => run_validate(&file),
        Commands::Plan { file } => run_plan(&file).await,
        Commands::Apply { file } => run_apply(&file).await,
        Commands::Fmt {
            path,
            check,
            diff,
            recursive,
        } => run_fmt(&path, check, diff, recursive),
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

fn run_validate(file: &PathBuf) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let mut parsed =
        parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

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

    println!("{}", "Execution Plan:".cyan().bold());
    println!();

    for effect in plan.effects() {
        match effect {
            Effect::Create(r) => {
                println!(
                    "  {} {}.{}",
                    "+".green().bold(),
                    r.id.resource_type,
                    r.id.name
                );
                for (key, value) in &r.attributes {
                    if !key.starts_with('_') {
                        println!(
                            "      {}: {}",
                            key,
                            format_value_with_key(value, Some(key)).green()
                        );
                    }
                }
            }
            Effect::Update { id, from, to, .. } => {
                println!("  {} {}.{}", "~".yellow().bold(), id.resource_type, id.name);
                for (key, new_value) in &to.attributes {
                    if !key.starts_with('_') {
                        let old_value = from.attributes.get(key);
                        if old_value != Some(new_value) {
                            let old_str = old_value
                                .map(|v| format_value_with_key(v, Some(key)))
                                .unwrap_or_else(|| "(none)".to_string());
                            println!(
                                "      {}: {} → {}",
                                key,
                                old_str.red(),
                                format_value_with_key(new_value, Some(key)).green()
                            );
                        }
                    }
                }
            }
            Effect::Delete(id) => {
                println!("  {} {}.{}", "-".red().bold(), id.resource_type, id.name);
            }
            Effect::Read(_) => {}
        }
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
