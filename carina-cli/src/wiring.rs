use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::identifier::{self, AnonymousIdStateInfo, PrefixStateInfo};
use carina_core::module_resolver;
use carina_core::parser::{ParsedFile, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::provider::{
    self as provider_mod, Provider, ProviderError, ProviderFactory, ProviderNormalizer,
    ProviderRouter,
};
use carina_core::resolver::resolve_refs_with_state;
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
/// (e.g., `UnresolvedIdent` -> namespaced enum strings) without requiring
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
        resolve_attrs_aliases(
            &mut resource.attributes,
            &resource.id.resource_type,
            factory,
        );
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

/// Merge default_tags from provider configs into resources that support tags.
///
/// For each resource whose schema includes a `tags` attribute:
/// - If the resource has no `tags`, set it to `default_tags`
/// - If the resource has `tags`, merge default_tags into it (resource-level tags win on conflict)
///
/// Resources whose schema does not include `tags` are skipped.
pub fn merge_default_tags(
    ctx: &WiringContext,
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) {
    // Build a map of provider name -> default_tags for quick lookup
    let provider_tags: HashMap<&str, &HashMap<String, Value>> = providers
        .iter()
        .filter(|p| !p.default_tags.is_empty())
        .map(|p| (p.name.as_str(), &p.default_tags))
        .collect();

    if provider_tags.is_empty() {
        return;
    }

    for resource in resources.iter_mut() {
        let default_tags = match provider_tags.get(resource.id.provider.as_str()) {
            Some(tags) => *tags,
            None => continue,
        };

        // Check if the resource schema has a `tags` attribute
        let schema_key = provider_mod::schema_key_for_resource(ctx.factories(), resource);
        let has_tags = ctx
            .schemas()
            .get(&schema_key)
            .is_some_and(|s| s.attributes.contains_key("tags"));

        if !has_tags {
            continue;
        }

        // Merge default_tags into the resource's tags
        let mut default_tag_keys: Vec<String> = Vec::new();
        match resource.attributes.get_mut("tags") {
            Some(Value::Map(existing_tags)) => {
                // Resource-level tags take precedence: only insert defaults for missing keys
                for (key, value) in default_tags {
                    if !existing_tags.contains_key(key) {
                        existing_tags.insert(key.clone(), value.clone());
                        default_tag_keys.push(key.clone());
                    }
                }
            }
            None => {
                // No tags on the resource: use default_tags as-is
                default_tag_keys = default_tags.keys().cloned().collect();
                resource
                    .attributes
                    .insert("tags".to_string(), Value::Map(default_tags.clone()));
            }
            _ => {
                // tags is some other value type (unexpected), skip
                continue;
            }
        }

        // Store which tag keys came from default_tags as internal metadata
        if !default_tag_keys.is_empty() {
            default_tag_keys.sort();
            resource.attributes.insert(
                "_default_tag_keys".to_string(),
                Value::List(default_tag_keys.into_iter().map(Value::String).collect()),
            );
        }
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

pub async fn create_plan_from_parsed(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
    refresh: bool,
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
        let provider_ref = &provider;
        let results: Vec<Result<(ResourceId, State), AppError>> = stream::iter(&sorted_resources)
            .map(|resource| {
                let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                let identifier = state_file
                    .as_ref()
                    .and_then(|sf| sf.get_identifier_for_resource(resource));
                async move {
                    let state = read_with_retry(provider_ref, &resource.id, identifier.as_deref())
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
    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    provider.hydrate_read_state(&mut current_states, &saved_attrs);

    // Resolve ResourceRef values and enum identifiers using AWS state
    let mut resources = sorted_resources.clone();
    resolve_refs_with_state(&mut resources, &current_states);
    provider.normalize_desired(&mut resources);

    // Normalize state enum values to match the DSL format produced by normalize_desired.
    // Without this, raw AWS values (e.g., "ap-northeast-1a") in state would diff against
    // normalized desired values (e.g., "awscc.ec2.subnet.AvailabilityZone.ap_northeast_1a").
    provider.normalize_state(&mut current_states);

    // Merge default_tags from provider configs into resources that support tags.
    // Done after normalize_desired so enum values in tags are already resolved.
    merge_default_tags(&ctx, &mut resources, &parsed.providers);

    // Resolve enum aliases (e.g., "all" -> "-1") in both desired resources
    // and current states so the differ sees canonical AWS values.
    resolve_enum_aliases_with_ctx(&ctx, &mut resources);
    resolve_enum_aliases_in_states(&ctx, &mut current_states);

    // Build lifecycles map from state file for orphaned resource deletion
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();

    let prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
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

    Ok(PlanContext {
        plan,
        sorted_resources,
        current_states,
    })
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
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].attributes.get("ip_protocol"),
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
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.tcp".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        // "tcp" has no alias, so it remains as the namespaced DSL value
        assert_eq!(
            resources[0].attributes.get("ip_protocol"),
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
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::String("aws.ec2.security_group_ingress.IpProtocol.all".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].attributes.get("ip_protocol"),
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
        resource.attributes.insert(
            "security_group_egress".to_string(),
            Value::List(vec![Value::Map(egress_map)]),
        );

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        if let Value::List(items) = &resources[0].attributes["security_group_egress"] {
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

    #[test]
    fn test_resolve_enum_aliases_non_enum_values_unchanged() {
        // Non-DSL-enum strings should not be affected
        let mut resource = Resource::with_provider("awscc", "ec2.security_group", "test-sg");
        resource.attributes.insert(
            "group_description".to_string(),
            Value::String("My security group".to_string()),
        );
        resource
            .attributes
            .insert("vpc_id".to_string(), Value::String("vpc-12345".to_string()));

        let mut resources = vec![resource];
        resolve_enum_aliases(&mut resources);

        assert_eq!(
            resources[0].attributes.get("group_description"),
            Some(&Value::String("My security group".to_string())),
        );
        assert_eq!(
            resources[0].attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345".to_string())),
        );
    }

    #[test]
    fn test_merge_default_tags_resource_tags_win() {
        let ctx = WiringContext::new();
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut resource_tags = HashMap::new();
        resource_tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        resource_tags.insert(
            "Environment".to_string(),
            Value::String("staging".to_string()),
        );
        resource
            .attributes
            .insert("tags".to_string(), Value::Map(resource_tags));

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );
        default_tags.insert("Team".to_string(), Value::String("platform".to_string()));

        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags,
        }];

        let mut resources = vec![resource];
        merge_default_tags(&ctx, &mut resources, &providers);

        if let Some(Value::Map(tags)) = resources[0].attributes.get("tags") {
            // Resource-level "Environment" should win
            assert_eq!(
                tags.get("Environment"),
                Some(&Value::String("staging".to_string()))
            );
            // Resource-level "Name" preserved
            assert_eq!(tags.get("Name"), Some(&Value::String("my-vpc".to_string())));
            // Default "Team" added
            assert_eq!(
                tags.get("Team"),
                Some(&Value::String("platform".to_string()))
            );
        } else {
            panic!("Expected tags to be a Map");
        }

        // Only "Team" should be recorded as a default_tag_key (Environment was overridden)
        if let Some(Value::List(keys)) = resources[0].attributes.get("_default_tag_keys") {
            let key_strs: Vec<&str> = keys
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(key_strs, vec!["Team"]);
        } else {
            panic!("Expected _default_tag_keys to be set");
        }
    }

    #[test]
    fn test_merge_default_tags_no_explicit_tags() {
        let ctx = WiringContext::new();
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        // No tags attribute on the resource

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );

        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags,
        }];

        let mut resources = vec![resource];
        merge_default_tags(&ctx, &mut resources, &providers);

        if let Some(Value::Map(tags)) = resources[0].attributes.get("tags") {
            assert_eq!(
                tags.get("Environment"),
                Some(&Value::String("production".to_string()))
            );
        } else {
            panic!("Expected tags to be set from default_tags");
        }

        // All tags came from defaults
        if let Some(Value::List(keys)) = resources[0].attributes.get("_default_tag_keys") {
            let key_strs: Vec<&str> = keys
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(key_strs, vec!["Environment"]);
        } else {
            panic!("Expected _default_tag_keys to be set");
        }
    }

    #[test]
    fn test_merge_default_tags_skips_no_tag_schema() {
        let ctx = WiringContext::new();
        // iam.role does not have a "tags" attribute in schema (it uses "tags" but let's
        // use a resource type that definitely doesn't have tags to test skip behavior).
        // ec2.route has no tags attribute.
        let mut resource = Resource::with_provider("awscc", "ec2.route", "test-route");
        resource.attributes.insert(
            "route_table_id".to_string(),
            Value::String("rtb-123".to_string()),
        );

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );

        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags,
        }];

        let mut resources = vec![resource];
        merge_default_tags(&ctx, &mut resources, &providers);

        // Should not have tags since ec2.route schema doesn't support tags
        assert!(!resources[0].attributes.contains_key("tags"));
        assert!(!resources[0].attributes.contains_key("_default_tag_keys"));
    }

    #[test]
    fn test_merge_default_tags_no_default_tags() {
        let ctx = WiringContext::new();
        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut resource_tags = HashMap::new();
        resource_tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        resource
            .attributes
            .insert("tags".to_string(), Value::Map(resource_tags));

        let providers = vec![ProviderConfig {
            name: "awscc".to_string(),
            attributes: HashMap::new(),
            default_tags: HashMap::new(),
        }];

        let mut resources = vec![resource];
        merge_default_tags(&ctx, &mut resources, &providers);

        // Tags should be unchanged since there are no default_tags
        if let Some(Value::Map(tags)) = resources[0].attributes.get("tags") {
            assert_eq!(tags.len(), 1);
            assert_eq!(tags.get("Name"), Some(&Value::String("my-vpc".to_string())));
        } else {
            panic!("Expected tags to be unchanged");
        }
        // No _default_tag_keys when there are no default_tags
        assert!(!resources[0].attributes.contains_key("_default_tag_keys"));
    }
}
