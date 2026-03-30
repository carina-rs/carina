use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::effect::Effect;
use carina_core::identifier::{self, AnonymousIdStateInfo, PrefixStateInfo};
use carina_core::module_resolver;
use carina_core::parser::{ParsedFile, ProviderConfig, StateBlock};
use carina_core::plan::Plan;
use carina_core::provider::{
    self as provider_mod, Provider, ProviderError, ProviderFactory, ProviderNormalizer,
    ProviderRouter,
};
use carina_core::resolver::resolve_refs_with_state_and_remote;
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::{ResourceSchema, resolve_block_names};
use carina_core::utils;
use carina_core::validation;
use carina_provider_aws::AwsProviderFactory;
use carina_provider_awscc::AwsccProviderFactory;
use carina_provider_mock::MockProvider;
use carina_state::StateFile;

use crate::commands::apply::{RefreshProgress, refresh_multi_progress};
use crate::error::AppError;

/// Result of creating a plan, with context needed for saving
pub struct PlanContext {
    pub plan: Plan,
    pub sorted_resources: Vec<Resource>,
    pub current_states: HashMap<ResourceId, State>,
    /// Maps moved-to resource IDs to their original (moved-from) IDs.
    /// Used by display to show "(moved from: ...)" annotations on Update/Replace effects.
    pub moved_origins: HashMap<ResourceId, ResourceId>,
}

/// Cached provider factories and schemas, constructed once per CLI invocation.
///
/// Instead of calling `provider_factories()` and `get_schemas()` at each call
/// site (which rebuilds the full schema set every time), create a single
/// `WiringContext` and pass it through the command execution path.
pub struct WiringContext {
    factories: Vec<Box<dyn ProviderFactory>>,
    schemas: HashMap<String, ResourceSchema>,
}

impl WiringContext {
    pub fn new() -> Self {
        let factories: Vec<Box<dyn ProviderFactory>> =
            vec![Box::new(AwsProviderFactory), Box::new(AwsccProviderFactory)];
        let schemas = provider_mod::collect_schemas(&factories);
        Self { factories, schemas }
    }

    pub fn factories(&self) -> &[Box<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn schemas(&self) -> &HashMap<String, ResourceSchema> {
        &self.schemas
    }
}

pub fn validate_resources_with_ctx(
    ctx: &WiringContext,
    resources: &[Resource],
) -> Result<(), AppError> {
    validation::validate_resources(resources, ctx.schemas(), &|r| {
        provider_mod::schema_key_for_resource(ctx.factories(), r)
    })
    .map_err(AppError::Validation)
}

pub fn validate_resource_ref_types_with_ctx(
    ctx: &WiringContext,
    resources: &[Resource],
    argument_names: &HashSet<String>,
) -> Result<(), AppError> {
    validation::validate_resource_ref_types(
        resources,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        argument_names,
    )
    .map_err(AppError::Validation)
}

/// Resolve block name aliases and attribute prefixes in one step.
pub fn resolve_names_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
) -> Result<(), AppError> {
    resolve_block_names(resources, ctx.schemas(), |r| {
        provider_mod::schema_key_for_resource(ctx.factories(), r)
    })
    .map_err(AppError::Validation)?;
    resolve_attr_prefixes_with_ctx(ctx, resources)
}

pub fn resolve_attr_prefixes_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
) -> Result<(), AppError> {
    identifier::resolve_attr_prefixes(resources, ctx.schemas(), &|r| {
        provider_mod::schema_key_for_resource(ctx.factories(), r)
    })
    .map_err(AppError::Validation)
}

pub fn reconcile_prefixed_names(resources: &mut [Resource], state_file: &Option<StateFile>) {
    let state_file = match state_file {
        Some(sf) => sf,
        None => return,
    };

    identifier::reconcile_prefixed_names(resources, &|provider, resource_type, name| {
        let sr = state_file.find_resource(provider, resource_type, name)?;
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

pub fn reconcile_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
    state_file: &StateFile,
) {
    identifier::reconcile_anonymous_identifiers(
        resources,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        &|provider, resource_type| {
            let schema_key = format!("{}.{}", provider, resource_type);
            let create_only_attrs = ctx
                .schemas()
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

pub fn compute_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    identifier::compute_anonymous_identifiers(
        resources,
        providers,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        &|name| {
            provider_mod::find_factory(ctx.factories(), name)
                .map(|f| {
                    f.identity_attributes()
                        .into_iter()
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default()
        },
    )
    .map_err(AppError::Config)
}

/// Run provider-specific normalization on desired resources.
///
/// Creates normalizers from all registered provider factories and applies
/// `normalize_desired()` to the resources. This resolves enum identifiers
/// (e.g., bare enum identifiers -> namespaced enum strings) without requiring
/// actual provider instances or network access.
#[cfg(test)]
pub fn normalize_desired_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build tokio runtime for normalize_desired");
    let mut router = ProviderRouter::new();
    for factory in ctx.factories() {
        let attrs = HashMap::new();
        if let Some(normalizer) = rt.block_on(factory.create_normalizer(&attrs)) {
            router.add_normalizer(normalizer);
        }
    }
    router.normalize_desired(resources);
}

/// Normalize enum values in current states to match DSL format.
///
/// Creates normalizers from all registered provider factories and applies
/// `normalize_state()` to the current states. This converts raw AWS enum
/// values (e.g., `"ap-northeast-1a"`) to the same DSL format that
/// `normalize_desired` produces, preventing false diffs.
#[cfg(test)]
pub fn normalize_state_with_ctx(
    ctx: &WiringContext,
    current_states: &mut HashMap<ResourceId, State>,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build tokio runtime for normalize_state");
    let mut router = ProviderRouter::new();
    for factory in ctx.factories() {
        let attrs = HashMap::new();
        if let Some(normalizer) = rt.block_on(factory.create_normalizer(&attrs)) {
            router.add_normalizer(normalizer);
        }
    }
    router.normalize_state(current_states);
}

/// Resolve enum alias values in resources to their canonical AWS form.
///
/// After `normalize_desired()` converts DSL identifiers to namespaced strings
/// (e.g., `IpProtocol.all` -> `"awscc.ec2.security_group_egress.IpProtocol.all"`),
/// this function resolves aliases to their canonical AWS values
/// (e.g., `"all"` -> `"-1"`).
///
/// This must be called on both desired resources and current states to ensure
/// the differ sees consistent values and produces no false diffs.
pub fn resolve_enum_aliases_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) {
    for resource in resources.iter_mut() {
        if resource.id.provider.is_empty() {
            continue;
        }
        let factory = match provider_mod::find_factory(ctx.factories(), &resource.id.provider) {
            Some(f) => f,
            None => continue,
        };
        let mut value_attrs = resource.resolved_attributes();
        resolve_attrs_aliases(&mut value_attrs, &resource.id.resource_type, factory);
        resource.attributes = carina_core::resource::Expr::wrap_map(value_attrs);
    }
}

/// Resolve enum alias values in current states to their canonical AWS form.
///
/// Ensures that read-back state attributes use canonical AWS values (e.g., `"-1"`)
/// instead of DSL aliases (e.g., `"all"`), matching the resolved desired values.
pub fn resolve_enum_aliases_in_states(
    ctx: &WiringContext,
    current_states: &mut HashMap<ResourceId, State>,
) {
    for (id, state) in current_states.iter_mut() {
        if !state.exists || id.provider.is_empty() {
            continue;
        }
        let factory = match provider_mod::find_factory(ctx.factories(), &id.provider) {
            Some(f) => f,
            None => continue,
        };
        resolve_attrs_aliases(&mut state.attributes, &id.resource_type, factory);
    }
}

/// Resolve enum aliases in an attribute map.
fn resolve_attrs_aliases(
    attrs: &mut HashMap<String, Value>,
    resource_type: &str,
    factory: &dyn ProviderFactory,
) {
    let keys: Vec<String> = attrs.keys().cloned().collect();
    for key in keys {
        if let Some(value) = attrs.get_mut(&key) {
            resolve_value_alias(value, resource_type, &key, factory);
        }
    }
}

/// Resolve a single value's enum alias, recursing into lists and maps.
fn resolve_value_alias(
    value: &mut Value,
    resource_type: &str,
    attr_name: &str,
    factory: &dyn ProviderFactory,
) {
    match value {
        Value::String(s) if utils::is_dsl_enum_format(s) => {
            let raw = utils::convert_enum_value(s);
            if let Some(canonical) = factory.get_enum_alias_reverse(resource_type, attr_name, &raw)
            {
                *s = canonical.to_string();
            }
        }
        Value::List(items) => {
            for item in items.iter_mut() {
                resolve_value_alias(item, resource_type, attr_name, factory);
            }
        }
        Value::Map(map) => {
            let map_keys: Vec<String> = map.keys().cloned().collect();
            for map_key in map_keys {
                if let Some(v) = map.get_mut(&map_key) {
                    resolve_value_alias(v, resource_type, &map_key, factory);
                }
            }
        }
        _ => {}
    }
}

pub fn check_unused_bindings(parsed: &ParsedFile) -> Vec<String> {
    validation::check_unused_bindings(parsed)
}

pub fn validate_provider_region_with_ctx(
    ctx: &WiringContext,
    parsed: &ParsedFile,
) -> Result<(), AppError> {
    validation::validate_provider_config(parsed, ctx.factories()).map_err(AppError::Validation)
}

pub fn validate_module_calls(parsed: &ParsedFile, base_dir: &Path) -> Result<(), AppError> {
    let mut imported_modules = HashMap::new();
    for import in &parsed.imports {
        let module_path = base_dir.join(&import.path);
        if let Some(module_parsed) = module_resolver::load_module(&module_path) {
            imported_modules.insert(import.alias.clone(), module_parsed.arguments);
        }
    }

    validation::validate_module_calls(&parsed.module_calls, &imported_modules)
        .map_err(AppError::Validation)
}

pub async fn get_provider_with_ctx(ctx: &WiringContext, parsed: &ParsedFile) -> ProviderRouter {
    let mut router = ProviderRouter::new();

    for provider_config in &parsed.providers {
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&provider_config.attributes).await;
            router.add_provider(provider_config.name.clone(), provider);
            if let Some(ext) = factory.create_normalizer(&provider_config.attributes).await {
                router.add_normalizer(ext);
            }
        }
    }

    if router.is_empty() {
        // Use mock provider for other cases.
        // Register with empty key to match resources without a provider prefix.
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    router
}

pub async fn create_providers_from_configs(configs: &[ProviderConfig]) -> ProviderRouter {
    let ctx = WiringContext::new();
    let mut router = ProviderRouter::new();

    for config in configs {
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &config.name) {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&config.attributes).await;
            router.add_provider(config.name.clone(), provider);
            if let Some(ext) = factory.create_normalizer(&config.attributes).await {
                router.add_normalizer(ext);
            }
        }
    }

    if router.is_empty() {
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    router
}

/// Create a plan from parsed configuration (without remote state bindings).
///
/// This is a convenience wrapper around `create_plan_from_parsed_with_remote`
/// for callers that don't use remote_state data sources.
#[allow(dead_code)]
pub async fn create_plan_from_parsed(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
    refresh: bool,
) -> Result<PlanContext, AppError> {
    create_plan_from_parsed_with_remote(parsed, state_file, refresh, &HashMap::new()).await
}

pub async fn create_plan_from_parsed_with_remote(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
    refresh: bool,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
) -> Result<PlanContext, AppError> {
    let ctx = WiringContext::new();
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(&ctx, parsed).await;

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();

    if refresh {
        RefreshProgress::start_header();
        let multi = refresh_multi_progress();

        // Read states for all resources concurrently using identifier from state.
        // In identifier-based approach, if there's no identifier in state, the resource doesn't exist.
        // Skip virtual resources (module attribute containers) — they have no infrastructure.
        let provider_ref = &provider;
        let results: Vec<Result<(ResourceId, State), AppError>> =
            stream::iter(sorted_resources.iter().filter(|r| !r.is_virtual()))
                .map(|resource| {
                    let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                    let identifier = state_file
                        .as_ref()
                        .and_then(|sf| sf.get_identifier_for_resource(resource));
                    async move {
                        let state =
                            read_with_retry(provider_ref, &resource.id, identifier.as_deref())
                                .await
                                .map_err(AppError::Provider)?;
                        progress.finish();
                        Ok((resource.id.clone(), state))
                    }
                })
                .buffer_unordered(5)
                .collect()
                .await;
        for result in results {
            let (id, state) = result?;
            current_states.insert(id, state);
        }

        // Seed current_states with orphaned resources from state file (#844).
        // These are resources tracked in state but removed from the .crn config.
        // Refresh each orphan via provider.read() concurrently to verify it still exists (#931).
        if let Some(sf) = state_file.as_ref() {
            let desired_ids: HashSet<ResourceId> =
                sorted_resources.iter().map(|r| r.id.clone()).collect();
            let orphan_states: Vec<(ResourceId, State)> =
                sf.build_orphan_states(&desired_ids).into_iter().collect();
            let orphan_results: Vec<Result<(ResourceId, State), AppError>> =
                stream::iter(orphan_states)
                    .map(|(id, state)| {
                        let progress = RefreshProgress::begin_multi(&multi, &id);
                        async move {
                            let refreshed =
                                read_with_retry(provider_ref, &id, state.identifier.as_deref())
                                    .await
                                    .map_err(AppError::Provider)?;
                            progress.finish();
                            Ok((id, refreshed))
                        }
                    })
                    .buffer_unordered(5)
                    .collect()
                    .await;
            for result in orphan_results {
                let (id, refreshed) = result?;
                if refreshed.exists {
                    current_states.entry(id).or_insert(refreshed);
                }
            }
        }
    } else {
        // --refresh=false: use cached state from state file instead of calling provider.read()
        if let Some(sf) = state_file.as_ref() {
            for resource in &sorted_resources {
                let state = sf.build_state_for_resource(resource);
                current_states.insert(resource.id.clone(), state);
            }

            // Also include orphaned resources from state file
            let desired_ids: HashSet<ResourceId> =
                sorted_resources.iter().map(|r| r.id.clone()).collect();
            for (id, state) in sf.build_orphan_states(&desired_ids) {
                current_states.entry(id).or_insert(state);
            }
        } else {
            // No state file: all resources are new (not found)
            for resource in &sorted_resources {
                current_states.insert(resource.id.clone(), State::not_found(resource.id.clone()));
            }
        }
    }

    // Build orphan dependency bindings from state file for tree structure
    let orphan_dependencies = if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        sf.build_orphan_dependencies(&desired_ids)
    } else {
        HashMap::new()
    };

    // Restore unreturned attributes from state file (CloudControl doesn't always return them)
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.hydrate_read_state(&mut current_states, &saved_attrs);

    // Resolve ResourceRef values and enum identifiers using AWS state
    let mut resources = sorted_resources.clone();
    resolve_refs_with_state_and_remote(&mut resources, &current_states, remote_bindings)?;
    provider.normalize_desired(&mut resources);

    // Normalize state enum values to match the DSL format produced by normalize_desired.
    // Without this, raw AWS values (e.g., "ap-northeast-1a") in state would diff against
    // normalized desired values (e.g., "awscc.ec2.subnet.AvailabilityZone.ap_northeast_1a").
    provider.normalize_state(&mut current_states);

    // Merge default_tags from provider configs into resources that support tags.
    // Done after normalize_desired so enum values in tags are already resolved.
    for provider_config in &parsed.providers {
        if !provider_config.default_tags.is_empty() {
            provider.merge_default_tags(
                &mut resources,
                &provider_config.default_tags,
                ctx.schemas(),
            );
        }
    }

    // Resolve enum aliases (e.g., "all" -> "-1") in both desired resources
    // and current states so the differ sees canonical AWS values.
    resolve_enum_aliases_with_ctx(&ctx, &mut resources);
    resolve_enum_aliases_in_states(&ctx, &mut current_states);

    // Build prev_desired_keys before moved-state transfer
    // so materialize_moved_states can re-key them under the new resource name.
    let mut prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();

    // Pre-process moved blocks: transfer state, prev_desired_keys, and
    // saved_attrs from old name to new name so the differ sees attribute
    // changes (including removals) and produces Update/Replace effects.
    let moved_pairs = materialize_moved_states(
        &mut current_states,
        &mut prev_desired_keys,
        &mut saved_attrs,
        &parsed.state_blocks,
        state_file,
    );

    // Build lifecycles map from state file for orphaned resource deletion
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();
    let mut plan = create_plan(
        &resources,
        &current_states,
        &lifecycles,
        ctx.schemas(),
        &saved_attrs,
        &prev_desired_keys,
        &orphan_dependencies,
    );

    // Populate cascading updates for Replace effects with create_before_destroy.
    // Uses unresolved resources (sorted_resources) so dependent Update effects
    // retain ResourceRef values for re-resolution at apply time.
    cascade_dependent_updates(&mut plan, &sorted_resources, &current_states, ctx.schemas());

    // Add state block effects (import/removed/moved) to the plan
    add_state_block_effects(&mut plan, &parsed.state_blocks, state_file, &moved_pairs);

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();

    Ok(PlanContext {
        plan,
        sorted_resources,
        current_states,
        moved_origins,
    })
}

/// Pre-process moved blocks by transferring state, `prev_desired_keys`, and
/// `saved_attrs` from the old resource name to the new name.
///
/// This must be called BEFORE `create_plan()` so the differ sees the moved
/// resource's state under its new name and can produce Update/Replace effects
/// if attributes differ between state and desired. Transferring
/// `prev_desired_keys` ensures attribute removals are detected; transferring
/// `saved_attrs` ensures hydrated attributes are found under the new name.
///
/// Returns a list of active Move pairs (from, to) where the `from` resource
/// existed in state. Callers use this to add Move effects to the plan.
pub fn materialize_moved_states(
    current_states: &mut HashMap<ResourceId, State>,
    prev_desired_keys: &mut HashMap<ResourceId, Vec<String>>,
    saved_attrs: &mut HashMap<ResourceId, HashMap<String, Value>>,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
) -> Vec<(ResourceId, ResourceId)> {
    let mut moved_pairs = Vec::new();

    for block in state_blocks {
        if let StateBlock::Moved { from, to } = block {
            let old_in_state = state_file.as_ref().is_some_and(|sf| {
                sf.find_resource(&from.provider, &from.resource_type, &from.name)
                    .is_some()
            });
            if old_in_state {
                // Transfer state from the old name to the new name so the
                // differ compares desired(to) against actual(from).
                if let Some(mut state) = current_states.remove(from) {
                    state.id = to.clone();
                    current_states.insert(to.clone(), state);
                }

                // Transfer prev_desired_keys so the differ detects attribute
                // removals under the new resource name.
                if let Some(keys) = prev_desired_keys.remove(from) {
                    prev_desired_keys.insert(to.clone(), keys);
                }

                // Transfer saved_attrs so create_plan can look up saved
                // attributes under the new resource name.
                if let Some(attrs) = saved_attrs.remove(from) {
                    saved_attrs.insert(to.clone(), attrs);
                }

                moved_pairs.push((from.clone(), to.clone()));
            }
        }
    }

    moved_pairs
}

/// Add state block effects (import/removed/moved) to the plan.
///
/// State blocks become no-ops on subsequent runs when:
/// - Import: the resource already exists in state
/// - Removed: the resource does not exist in state
/// - Moved: the old resource does not exist in state (already moved)
///
/// For `moved` blocks, `materialize_moved_states()` must be called before
/// `create_plan()` to transfer state entries. The pre-computed `moved_pairs`
/// are passed here to add Move effects without re-checking state.
///
/// This function also removes Delete effects for resources covered by
/// `removed` blocks, since those operations manage state without
/// destroying infrastructure.
pub fn add_state_block_effects(
    plan: &mut Plan,
    state_blocks: &[StateBlock],
    state_file: &Option<StateFile>,
    moved_pairs: &[(ResourceId, ResourceId)],
) {
    // Collect resource IDs that are covered by removed blocks
    // to suppress orphan Delete effects
    let mut suppress_delete: std::collections::HashSet<ResourceId> =
        std::collections::HashSet::new();
    let mut suppress_create: std::collections::HashSet<ResourceId> =
        std::collections::HashSet::new();

    let mut new_effects: Vec<Effect> = Vec::new();

    for block in state_blocks {
        match block {
            StateBlock::Import { to, id } => {
                // Skip if resource already exists in state
                let already_in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(&to.provider, &to.resource_type, &to.name)
                        .is_some()
                });
                if !already_in_state {
                    suppress_create.insert(to.clone());
                    new_effects.push(Effect::Import {
                        id: to.clone(),
                        identifier: id.clone(),
                    });
                }
            }
            StateBlock::Removed { from } => {
                // Skip if resource is not in state
                let in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(&from.provider, &from.resource_type, &from.name)
                        .is_some()
                });
                if in_state {
                    suppress_delete.insert(from.clone());
                    new_effects.push(Effect::Remove { id: from.clone() });
                }
            }
            StateBlock::Moved { .. } => {
                // Moved blocks are handled by materialize_moved_states() + moved_pairs below
            }
        }
    }

    // Add Move effects from pre-computed moved pairs.
    // Move effects are always added to the plan (for summary counting),
    // but display skips the Move line when an Update/Replace with
    // "(moved from: ...)" annotation already conveys the information.
    // Also suppress orphan Delete for `to` when there is no desired resource
    // for the target (the moved state entry would otherwise appear as an orphan).
    for (from, to) in moved_pairs {
        suppress_delete.insert(to.clone());
        new_effects.push(Effect::Move {
            from: from.clone(),
            to: to.clone(),
        });
    }

    // Remove Delete effects for resources covered by removed blocks,
    // and Create effects for import targets (they will be imported, not created)
    if !suppress_delete.is_empty() || !suppress_create.is_empty() {
        plan.retain(|effect| match effect {
            Effect::Delete { id, .. } => !suppress_delete.contains(id),
            Effect::Create(resource) => !suppress_create.contains(&resource.id),
            _ => true,
        });
    }

    // Add the new state block effects
    for effect in new_effects {
        plan.add(effect);
    }
}

/// Check whether a `ProviderError` is an AWS throttling error that should be retried.
fn is_throttling_error(err: &ProviderError) -> bool {
    let msg = err.to_string();
    msg.contains("ThrottlingException") || msg.contains("Rate exceeded")
}

/// Read a resource via the provider with retry and exponential backoff for throttling errors.
///
/// Retries up to 3 times with delays of 1s, 2s, 4s when the error looks like an
/// AWS throttling / rate-limit response.
pub async fn read_with_retry(
    provider: &dyn Provider,
    id: &ResourceId,
    identifier: Option<&str>,
) -> Result<State, ProviderError> {
    let max_retries = 3;
    for attempt in 0..=max_retries {
        match provider.read(id, identifier).await {
            Ok(state) => return Ok(state),
            Err(e) if attempt < max_retries && is_throttling_error(&e) => {
                let delay = Duration::from_secs(1 << attempt); // 1s, 2s, 4s
                eprintln!(
                    "  Throttled reading {}, retrying in {}s...",
                    id,
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Convenience wrappers for tests. Each creates a fresh `WiringContext` internally,
/// which is acceptable in test code where the overhead is negligible.
#[cfg(test)]
pub fn validate_resources(resources: &[Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new();
    validate_resources_with_ctx(&ctx, resources)
}

#[cfg(test)]
pub fn resolve_names(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new();
    resolve_names_with_ctx(&ctx, resources)
}

#[cfg(test)]
pub fn resolve_attr_prefixes(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new();
    resolve_attr_prefixes_with_ctx(&ctx, resources)
}

#[cfg(test)]
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let ctx = WiringContext::new();
    compute_anonymous_identifiers_with_ctx(&ctx, resources, providers)
}

#[cfg(test)]
pub fn resolve_enum_aliases(resources: &mut [Resource]) {
    let ctx = WiringContext::new();
    resolve_enum_aliases_with_ctx(&ctx, resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_enum_aliases_ip_protocol_all() {
        // After normalize_desired, ip_protocol "all" becomes a namespaced DSL value.
        // resolve_enum_aliases should resolve the alias "all" -> "-1".
        let mut resource =
            Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule");
        resource.set_attr(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].get_attr("ip_protocol"),
            Some(&Value::String("-1".to_string())),
            "Alias 'all' should be resolved to canonical AWS value '-1'"
        );
    }

    #[test]
    fn test_resolve_enum_aliases_no_alias() {
        // "tcp" has no alias mapping, so it should be converted from DSL enum
        // to its raw form by convert_enum_value but not further changed.
        let mut resource =
            Resource::with_provider("awscc", "ec2.security_group_egress", "test-rule");
        resource.set_attr(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        // "tcp" has no alias, so it remains as the namespaced DSL value
        assert_eq!(
            resources[0].get_attr("ip_protocol"),
            Some(&Value::String(
                "awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()
            )),
        );
    }

    #[test]
    fn test_resolve_enum_aliases_aws_provider() {
        // Same alias resolution should work for the aws provider
        let mut resource =
            Resource::with_provider("aws", "ec2.security_group_ingress", "test-rule");
        resource.set_attr(
            "ip_protocol".to_string(),
            Value::String("aws.ec2.security_group_ingress.IpProtocol.all".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].get_attr("ip_protocol"),
            Some(&Value::String("-1".to_string())),
        );
    }

    #[test]
    fn test_resolve_enum_aliases_in_states() {
        // Current states should also have aliases resolved
        let ctx = WiringContext::new();
        let id = ResourceId::with_provider("awscc", "ec2.security_group_egress", "test-rule");
        let mut attrs = HashMap::new();
        attrs.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
        );
        let state = State::existing(id.clone(), attrs);
        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        super::resolve_enum_aliases_in_states(&ctx, &mut current_states);

        assert_eq!(
            current_states[&id].attributes.get("ip_protocol"),
            Some(&Value::String("-1".to_string())),
        );
    }

    #[test]
    fn test_resolve_enum_aliases_in_struct_field() {
        // Aliases within struct fields (maps inside lists) should also be resolved
        let mut resource = Resource::with_provider("awscc", "ec2.security_group", "test-sg");
        let mut egress_map = HashMap::new();
        egress_map.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group.IpProtocol.all".to_string()),
        );
        egress_map.insert(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        resource.set_attr(
            "security_group_egress".to_string(),
            Value::List(vec![Value::Map(egress_map)]),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        if let Value::List(items) = resources[0].get_attr("security_group_egress").unwrap() {
            if let Value::Map(m) = &items[0] {
                assert_eq!(
                    m.get("ip_protocol"),
                    Some(&Value::String("-1".to_string())),
                    "Alias in struct field should be resolved"
                );
                assert_eq!(
                    m.get("cidr_ip"),
                    Some(&Value::String("0.0.0.0/0".to_string())),
                    "Non-alias values should not be changed"
                );
            } else {
                panic!("Expected Map in egress list");
            }
        } else {
            panic!("Expected List for security_group_egress");
        }
    }

    /// Verify that normalize_state prevents false diffs for enum values in state.
    ///
    /// When state contains raw AWS enum values (e.g., "default") and desired
    /// resources have been normalized to DSL enum format (e.g.,
    /// "awscc.ec2.vpc.InstanceTenancy.default"), the differ would see a false
    /// diff unless normalize_state is also applied to current states.
    ///
    /// Both the plan path (wiring.rs) and the apply path (apply.rs) must call
    /// normalize_state to maintain parity. This test ensures the normalization
    /// produces matching values so no false diff occurs.
    #[test]
    fn test_normalize_state_prevents_false_enum_diff() {
        use carina_core::differ::create_plan;
        use carina_core::resource::LifecycleConfig;
        use carina_core::schema::ResourceSchema;

        let ctx = WiringContext::new();

        // Desired resource with normalized DSL enum value (after normalize_desired)
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.set_attr(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );

        // State with raw AWS value (as returned by provider.read())
        let id = resource.id.clone();
        let mut state_attrs = HashMap::new();
        state_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("default".to_string()),
        );
        let state = State::existing(id.clone(), state_attrs);
        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        // Without normalize_state, the differ would see a false diff
        let resources_without = vec![resource.clone()];
        let lifecycles: HashMap<ResourceId, LifecycleConfig> = HashMap::new();
        let schemas: HashMap<String, ResourceSchema> = HashMap::new();
        let saved_attrs = HashMap::new();
        let prev_desired_keys = HashMap::new();
        let orphan_deps = HashMap::new();
        let plan_without = create_plan(
            &resources_without,
            &current_states,
            &lifecycles,
            &schemas,
            &saved_attrs,
            &prev_desired_keys,
            &orphan_deps,
        );
        assert!(
            !plan_without.is_empty(),
            "Without normalize_state, differ should see a false diff"
        );

        // After normalize_state, state values match desired values → no diff
        normalize_state_with_ctx(&ctx, &mut current_states);
        let resources_with = vec![resource];
        let plan_with = create_plan(
            &resources_with,
            &current_states,
            &lifecycles,
            &schemas,
            &saved_attrs,
            &prev_desired_keys,
            &orphan_deps,
        );
        assert!(
            plan_with.is_empty(),
            "After normalize_state, no false diff should occur"
        );
    }

    /// Verify that merge_default_tags prevents false diffs when default_tags are
    /// configured in the provider block.
    ///
    /// When default_tags are set and state already contains those tags (from a
    /// previous apply), the differ must not report a diff for the tags. This
    /// requires merge_default_tags to be called so the desired resources include
    /// the default tags before diffing.
    ///
    /// Both the plan path (wiring.rs) and the apply path (apply.rs) must call
    /// merge_default_tags to maintain parity.
    #[test]
    fn test_merge_default_tags_prevents_false_diff() {
        use carina_core::differ::create_plan;
        use carina_core::resource::LifecycleConfig;
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

        // Build a minimal schema that has a "tags" attribute.
        // merge_default_tags checks for the presence of "tags" in the schema.
        let schema = ResourceSchema::new("awscc.s3.bucket").attribute(AttributeSchema::new(
            "tags",
            AttributeType::Map(Box::new(AttributeType::String)),
        ));
        let mut schemas: HashMap<String, ResourceSchema> = HashMap::new();
        schemas.insert("awscc.s3.bucket".to_string(), schema);

        // Desired resource without explicit tags
        let resource = Resource::with_provider("awscc", "s3.bucket", "test-bucket");

        // State already has the default tags (from a previous apply)
        let id = resource.id.clone();
        let mut state_attrs = HashMap::new();
        let mut tags = HashMap::new();
        tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );
        state_attrs.insert("tags".to_string(), Value::Map(tags));
        let state = State::existing(id.clone(), state_attrs);
        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let default_tags: HashMap<String, Value> = {
            let mut m = HashMap::new();
            m.insert(
                "Environment".to_string(),
                Value::String("production".to_string()),
            );
            m
        };

        // Simulate prev_desired_keys from a previous apply that included "tags"
        // (because merge_default_tags was called correctly in the plan path).
        let mut prev_desired_keys = HashMap::new();
        prev_desired_keys.insert(resource.id.clone(), vec!["tags".to_string()]);

        // Without merge_default_tags, the desired resource has no tags,
        // but state has tags and prev_desired_keys says "tags" was previously desired.
        // The differ sees this as attribute removal → false Update diff.
        let resources_without = vec![resource.clone()];
        let lifecycles: HashMap<ResourceId, LifecycleConfig> = HashMap::new();
        let saved_attrs = HashMap::new();
        let orphan_deps = HashMap::new();
        let plan_without = create_plan(
            &resources_without,
            &current_states,
            &lifecycles,
            &schemas,
            &saved_attrs,
            &prev_desired_keys,
            &orphan_deps,
        );
        assert!(
            !plan_without.is_empty(),
            "Without merge_default_tags, differ should see a false diff"
        );

        // After merge_default_tags, desired resource gains the default tags → no diff
        let ctx = WiringContext::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build tokio runtime");
        let mut router = ProviderRouter::new();
        for factory in ctx.factories() {
            let attrs = HashMap::new();
            if let Some(normalizer) = rt.block_on(factory.create_normalizer(&attrs)) {
                router.add_normalizer(normalizer);
            }
        }
        let mut resources_with = vec![resource];
        router.merge_default_tags(&mut resources_with, &default_tags, &schemas);

        // After merging, desired now has tags matching state → no diff
        let plan_with = create_plan(
            &resources_with,
            &current_states,
            &lifecycles,
            &schemas,
            &saved_attrs,
            &prev_desired_keys,
            &orphan_deps,
        );
        assert!(
            plan_with.is_empty(),
            "After merge_default_tags, no false diff should occur"
        );
    }

    #[test]
    fn test_resolve_enum_aliases_non_enum_values_unchanged() {
        // Non-DSL-enum strings should not be affected
        let mut resource = Resource::with_provider("awscc", "ec2.security_group", "test-sg");
        resource.set_attr(
            "group_description".to_string(),
            Value::String("My security group".to_string()),
        );
        resource.set_attr("vpc_id".to_string(), Value::String("vpc-12345".to_string()));

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].get_attr("group_description"),
            Some(&Value::String("My security group".to_string())),
        );
        assert_eq!(
            resources[0].get_attr("vpc_id"),
            Some(&Value::String("vpc-12345".to_string())),
        );
    }
}
