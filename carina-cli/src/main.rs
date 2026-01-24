use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use colored::Colorize;
use similar::{ChangeTag, TextDiff};

use carina_core::differ::create_plan;
use carina_core::effect::Effect;
use carina_core::formatter::{self, FormatConfig};
use carina_core::interpreter::{EffectOutcome, Interpreter};
use carina_core::parser::{self, ParsedFile};
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult, ResourceType};
use carina_core::providers::s3;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

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
    let mut schemas = HashMap::new();
    for schema in s3::schemas() {
        schemas.insert(schema.resource_type.clone(), schema);
    }
    schemas
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

    let parsed = parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

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

    let parsed = parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    validate_resources(&parsed.resources)?;

    let plan = create_plan_from_parsed(&parsed).await?;
    print_plan(&plan);
    Ok(())
}

async fn run_apply(file: &PathBuf) -> Result<(), String> {
    let content = fs::read_to_string(file)
        .map_err(|e| format!("Failed to read {}: {}", file.display(), e))?;

    let parsed = parser::parse_and_resolve(&content).map_err(|e| format!("Parse error: {}", e))?;

    validate_resources(&parsed.resources)?;

    let plan = create_plan_from_parsed(&parsed).await?;

    if plan.is_empty() {
        println!("{}", "No changes needed.".green());
        return Ok(());
    }

    print_plan(&plan);
    println!();

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(&parsed).await;
    let interpreter = Interpreter::new(provider);

    println!("{}", "Applying changes...".cyan().bold());
    println!();

    let result = interpreter.apply(&plan).await;

    for (i, outcome) in result.outcomes.iter().enumerate() {
        let effect = &plan.effects()[i];
        match outcome {
            Ok(EffectOutcome::Created { .. }) => {
                println!("  {} {}", "✓".green(), format_effect(effect));
            }
            Ok(EffectOutcome::Updated { .. }) => {
                println!("  {} {}", "✓".green(), format_effect(effect));
            }
            Ok(EffectOutcome::Deleted) => {
                println!("  {} {}", "✓".green(), format_effect(effect));
            }
            Ok(EffectOutcome::Skipped { reason }) => {
                println!("  {} {} ({})", "⊘".yellow(), format_effect(effect), reason);
            }
            Ok(EffectOutcome::Read { .. }) => {}
            Err(e) => {
                println!("  {} {} - {}", "✗".red(), format_effect(effect), e);
            }
        }
    }

    println!();
    if result.is_success() {
        println!(
            "{}",
            format!("Apply complete! {} changes applied.", result.success_count)
                .green()
                .bold()
        );
    } else {
        println!(
            "{}",
            format!(
                "Apply failed. {} succeeded, {} failed.",
                result.success_count, result.failure_count
            )
            .red()
            .bold()
        );
    }

    Ok(())
}

/// Get region from provider configuration
fn get_aws_region(parsed: &ParsedFile) -> String {
    for provider in &parsed.providers {
        if provider.name == "aws"
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

async fn create_plan_from_parsed(parsed: &ParsedFile) -> Result<Plan, String> {
    let provider: Box<dyn Provider> = get_provider(parsed).await;

    // Get current state
    let mut current_states = HashMap::new();
    for resource in &parsed.resources {
        let state = provider
            .read(&resource.id)
            .await
            .map_err(|e| format!("Failed to read state: {}", e))?;
        current_states.insert(resource.id.clone(), state);
    }

    Ok(create_plan(&parsed.resources, &current_states))
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
                        println!("      {}: {}", key, format_value(value).green());
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
                                .map(format_value)
                                .unwrap_or_else(|| "(none)".to_string());
                            println!(
                                "      {}: {} → {}",
                                key,
                                old_str.red(),
                                format_value(new_value).green()
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

fn format_value(value: &Value) -> String {
    match value {
        Value::String(s) => format!("\"{}\"", s),
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
