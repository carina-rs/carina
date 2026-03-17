use std::collections::HashMap;
use std::path::Path;

use colored::Colorize;

use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::identifier::{self, AnonymousIdStateInfo, PrefixStateInfo};
use carina_core::module_resolver;
use carina_core::parser::{ParsedFile, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::provider::{self as provider_mod, Provider, ProviderFactory, ProviderRouter};
use carina_core::resolver::resolve_refs_with_state;
use carina_core::resource::{Resource, ResourceId, State};
use carina_core::schema::{ResourceSchema, resolve_block_names};
use carina_core::validation;
use carina_provider_aws::AwsProviderFactory;
use carina_provider_awscc::AwsccProviderFactory;
use carina_provider_mock::MockProvider;
use carina_state::StateFile;

use crate::error::AppError;

/// Result of creating a plan, with context needed for saving
pub struct PlanContext {
    pub plan: Plan,
    pub sorted_resources: Vec<Resource>,
    pub current_states: HashMap<ResourceId, State>,
}

pub fn provider_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(AwsProviderFactory), Box::new(AwsccProviderFactory)]
}

pub fn get_schemas() -> HashMap<String, ResourceSchema> {
    provider_mod::collect_schemas(&provider_factories())
}

pub fn validate_resources(resources: &[Resource]) -> Result<(), AppError> {
    let factories = provider_factories();
    let schemas = get_schemas();
    validation::validate_resources(resources, &schemas, &|r| {
        provider_mod::schema_key_for_resource(&factories, r)
    })
    .map_err(AppError::Validation)
}

pub fn validate_resource_ref_types(resources: &[Resource]) -> Result<(), AppError> {
    let factories = provider_factories();
    let schemas = get_schemas();
    validation::validate_resource_ref_types(resources, &schemas, &|r| {
        provider_mod::schema_key_for_resource(&factories, r)
    })
    .map_err(AppError::Validation)
}

/// Resolve block name aliases and attribute prefixes in one step.
pub fn resolve_names(resources: &mut [Resource]) -> Result<(), AppError> {
    let factories = provider_factories();
    let schemas = get_schemas();
    resolve_block_names(resources, &schemas, |r| {
        provider_mod::schema_key_for_resource(&factories, r)
    })
    .map_err(AppError::Validation)?;
    resolve_attr_prefixes(resources)
}

pub fn resolve_attr_prefixes(resources: &mut [Resource]) -> Result<(), AppError> {
    let factories = provider_factories();
    let schemas = get_schemas();
    identifier::resolve_attr_prefixes(resources, &schemas, &|r| {
        provider_mod::schema_key_for_resource(&factories, r)
    })
    .map_err(AppError::Validation)
}

pub fn reconcile_prefixed_names(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    identifier::reconcile_prefixed_names(resources, &|resource_type, name| {
        let sr = state_file.find_resource(resource_type, name)?;
        Some(PrefixStateInfo {
            prefixes: sr.prefixes.clone(),
            attribute_values: sr
                .attributes
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
        })
    });
}

pub fn reconcile_anonymous_identifiers(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    let factories = provider_factories();
    let schemas = get_schemas();
    identifier::reconcile_anonymous_identifiers(
        resources,
        &schemas,
        &|r| provider_mod::schema_key_for_resource(&factories, r),
        &|provider, resource_type| {
            // Look up schema to get create-only attribute names
            let schema_key = format!("{}.{}", provider, resource_type);
            let create_only_attrs = schemas
                .get(&schema_key)
                .map(|s| s.create_only_attributes())
                .unwrap_or_default();

            state_file
                .resources_by_type(provider, resource_type)
                .into_iter()
                .map(|sr| {
                    let create_only_values = create_only_attrs
                        .iter()
                        .filter_map(|attr| {
                            sr.attributes
                                .get(*attr)
                                .and_then(|v| v.as_str())
                                .map(|s| (attr.to_string(), s.to_string()))
                        })
                        .collect();
                    AnonymousIdStateInfo {
                        name: sr.name.clone(),
                        create_only_values,
                    }
                })
                .collect()
        },
    );
}

pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let factories = provider_factories();
    let schemas = get_schemas();
    identifier::compute_anonymous_identifiers(
        resources,
        providers,
        &schemas,
        &|r| provider_mod::schema_key_for_resource(&factories, r),
        &|name| {
            provider_mod::find_factory(&factories, name)
                .map(|f| {
                    f.identity_attributes()
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default()
        },
    )
    .map_err(AppError::Validation)
}

pub fn check_unused_bindings(parsed: &ParsedFile) -> Vec<String> {
    validation::check_unused_bindings(parsed)
}

pub fn validate_provider_region(parsed: &ParsedFile) -> Result<(), AppError> {
    let factories = provider_factories();
    validation::validate_provider_config(parsed, &factories).map_err(AppError::Validation)
}

pub fn validate_module_calls(parsed: &ParsedFile, base_dir: &Path) -> Result<(), AppError> {
    // Build a map of imported modules: alias -> inputs
    let mut imported_modules = HashMap::new();
    for import in &parsed.imports {
        let module_path = base_dir.join(&import.path);
        if let Some(module_parsed) = module_resolver::load_module(&module_path) {
            imported_modules.insert(import.alias.clone(), module_parsed.inputs);
        }
    }

    validation::validate_module_calls(&parsed.module_calls, &imported_modules)
        .map_err(AppError::Validation)
}

pub async fn get_provider(parsed: &ParsedFile) -> Box<dyn Provider> {
    let factories = provider_factories();
    let mut router = ProviderRouter::new();

    for provider_config in &parsed.providers {
        if let Some(factory) = provider_mod::find_factory(&factories, &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&provider_config.attributes).await;
            router.add_provider(provider_config.name.clone(), provider);
        }
    }

    if router.is_empty() {
        // Use mock provider for other cases
        println!("{}", "Using mock provider".cyan());
        Box::new(MockProvider::new())
    } else {
        Box::new(router)
    }
}

pub async fn create_providers_from_configs(configs: &[ProviderConfig]) -> Box<dyn Provider> {
    let factories = provider_factories();
    let mut router = ProviderRouter::new();

    for config in configs {
        if let Some(factory) = provider_mod::find_factory(&factories, &config.name) {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&config.attributes).await;
            router.add_provider(config.name.clone(), provider);
        }
    }

    if router.is_empty() {
        println!("{}", "Using mock provider".cyan());
        Box::new(MockProvider::new())
    } else {
        Box::new(router)
    }
}

pub async fn create_plan_from_parsed(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
) -> Result<PlanContext, AppError> {
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;

    // Select appropriate Provider based on configuration
    let provider: Box<dyn Provider> = get_provider(parsed).await;

    // Read states for all resources using identifier from state
    // In identifier-based approach, if there's no identifier in state, the resource doesn't exist
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    for resource in &sorted_resources {
        let identifier = state_file
            .as_ref()
            .and_then(|sf| sf.get_identifier_for_resource(resource));
        let state = provider.read(&resource.id, identifier.as_deref()).await?;
        current_states.insert(resource.id.clone(), state);
    }

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.restore_unreturned_attrs(&mut current_states, &saved_attrs);

    // Resolve ResourceRef values and enum identifiers using AWS state
    let mut resources = sorted_resources.clone();
    resolve_refs_with_state(&mut resources, &current_states);
    provider.resolve_enum_identifiers(&mut resources);

    // Build lifecycles map from state file for orphaned resource deletion
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();

    let schemas = get_schemas();
    let prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();
    let mut plan = create_plan(
        &resources,
        &current_states,
        &lifecycles,
        &schemas,
        &saved_attrs,
        &prev_desired_keys,
    );

    // Populate cascading updates for Replace effects with create_before_destroy.
    // Uses unresolved resources (sorted_resources) so dependent Update effects
    // retain ResourceRef values for re-resolution at apply time.
    cascade_dependent_updates(&mut plan, &sorted_resources, &current_states);

    Ok(PlanContext {
        plan,
        sorted_resources,
        current_states,
    })
}
