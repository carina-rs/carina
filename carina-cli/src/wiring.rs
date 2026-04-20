use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
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
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    schemas: HashMap<String, ResourceSchema>,
}

impl WiringContext {
    pub fn new(factories: Vec<Box<dyn ProviderFactory>>) -> Self {
        let schemas = provider_mod::collect_schemas(&factories);
        Self {
            factories: Arc::new(factories),
            schemas,
        }
    }

    pub fn factories(&self) -> &[Box<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn factories_arc(&self) -> Arc<Vec<Box<dyn ProviderFactory>>> {
        Arc::clone(&self.factories)
    }

    pub fn schemas(&self) -> &HashMap<String, ResourceSchema> {
        &self.schemas
    }
}

/// Build provider factories from provider configs that have a `source` attribute.
///
/// For each provider with a `source`, resolves the WASM component path and creates a
/// `WasmProviderFactory`. Providers without `source` are skipped (handled
/// later in `get_provider_with_ctx`).
///
/// Returns `(factories, load_errors)` where `load_errors` maps provider names
/// to their failure reasons, so callers can show accurate diagnostics.
pub fn build_factories_from_providers(
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> (Vec<Box<dyn ProviderFactory>>, HashMap<String, String>) {
    if let Err(e) = carina_provider_resolver::validate_lock_constraints(base_dir, providers) {
        eprintln!("{}", e.red());
        std::process::exit(1);
    }

    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
    let mut load_errors: HashMap<String, String> = HashMap::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s.clone(),
            None => continue,
        };

        let binary_path = if source.starts_with("file://") || source.starts_with("github.com/") {
            match carina_provider_resolver::find_installed_provider(base_dir, config) {
                Ok(path) => path,
                Err(e) => {
                    let reason = format!("Provider '{}' {}", config.name, e);
                    eprintln!("{}", reason.red());
                    load_errors.insert(config.name.clone(), reason);
                    continue;
                }
            }
        } else {
            let reason = format!(
                "Unsupported source format for provider '{}': {}. Use file:// or github.com/owner/repo.",
                config.name, source
            );
            eprintln!("{}", reason.red());
            load_errors.insert(config.name.clone(), reason);
            continue;
        };

        if !carina_provider_resolver::is_wasm_provider(&binary_path) {
            let reason = format!(
                "Provider '{}': native binaries are no longer supported. Use a .wasm component instead.",
                config.name
            );
            eprintln!("{}", reason.red());
            load_errors.insert(config.name.clone(), reason);
            continue;
        }

        let factory_result: Result<Box<dyn ProviderFactory>, String> = {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(
                    carina_plugin_host::WasmProviderFactory::new(binary_path.clone()),
                )
            })
            .and_then(|f| {
                if let Some(constraint) = &config.version {
                    f.verify_version(&constraint.raw)?;
                }
                Ok(Box::new(f) as Box<dyn ProviderFactory>)
            })
            .map_err(|e| format!("Failed to load WASM provider: {e}"))
        };

        match factory_result {
            Ok(factory) => {
                factories.push(factory);
            }
            Err(e) => {
                let reason = format!("Failed to load provider '{}': {}", config.name, e);
                eprintln!("{}", reason.red());
                load_errors.insert(config.name.clone(), reason);
            }
        }
    }

    (factories, load_errors)
}

/// Lift a core-validation `Result<(), String>` into per-finding
/// `AppError::Validation` entries. The underlying `validation::*`
/// helpers join every finding with `\n`, so splitting here lets
/// callers surface each finding as its own `AppError`.
fn lift_validation_result(res: Result<(), String>) -> Vec<AppError> {
    let Err(joined) = res else {
        return Vec::new();
    };
    joined
        .split('\n')
        .filter(|s| !s.is_empty())
        .map(|s| AppError::Validation(s.to_string()))
        .collect()
}

pub fn validate_resources_with_ctx(ctx: &WiringContext, parsed: &ParsedFile) -> Vec<AppError> {
    let known_providers: HashSet<String> = ctx
        .factories()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    lift_validation_result(validation::validate_resources(
        parsed,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        &known_providers,
    ))
}

pub fn validate_resource_ref_types_with_ctx(
    ctx: &WiringContext,
    parsed: &ParsedFile,
    argument_names: &HashSet<String>,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_resource_ref_types(
        parsed,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        argument_names,
    ))
}

pub fn validate_attribute_param_ref_types_with_ctx(
    ctx: &WiringContext,
    attribute_params: &[carina_core::parser::AttributeParameter],
    resources: &[Resource],
) -> Vec<AppError> {
    lift_validation_result(validation::validate_attribute_param_ref_types(
        attribute_params,
        resources,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
    ))
}

/// Resolve block name aliases and attribute prefixes in one step.
pub fn resolve_names_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) -> Vec<AppError> {
    let mut errors = lift_validation_result(resolve_block_names(resources, ctx.schemas(), |r| {
        provider_mod::schema_key_for_resource(ctx.factories(), r)
    }));
    errors.extend(resolve_attr_prefixes_with_ctx(ctx, resources));
    errors
}

pub fn resolve_attr_prefixes_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
) -> Vec<AppError> {
    lift_validation_result(identifier::resolve_attr_prefixes(
        resources,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
    ))
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

/// Look up a provider's identity attributes through its factory.
fn identity_attributes_for_provider(ctx: &WiringContext, name: &str) -> Vec<String> {
    provider_mod::find_factory(ctx.factories(), name)
        .map(|f| {
            f.identity_attributes()
                .into_iter()
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Detect and apply anonymous → let-bound resource renames.
///
/// Mirrors `materialize_moved_states` but for synthetic rename pairs produced
/// by `identifier::detect_anonymous_to_named_renames`. Transfers state,
/// `prev_desired_keys`, and `saved_attrs` from the old anonymous name to the
/// new binding name so the differ sees the resource under its new identity.
pub fn apply_anonymous_to_named_renames(
    ctx: &WiringContext,
    resources: &[Resource],
    providers: &[ProviderConfig],
    current_states: &mut HashMap<ResourceId, State>,
    prev_desired_keys: &mut HashMap<ResourceId, Vec<String>>,
    saved_attrs: &mut HashMap<ResourceId, HashMap<String, Value>>,
    state_file: &Option<StateFile>,
) -> Vec<(ResourceId, ResourceId)> {
    let Some(sf) = state_file.as_ref() else {
        return Vec::new();
    };

    let renames = identifier::detect_anonymous_to_named_renames(
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
            sf.resources_by_type(provider, resource_type)
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
        providers,
        &|name| identity_attributes_for_provider(ctx, name),
    );

    for (from, to) in &renames {
        if let Some(mut state) = current_states.remove(from) {
            state.id = to.clone();
            current_states.insert(to.clone(), state);
        }
        if let Some(keys) = prev_desired_keys.remove(from) {
            prev_desired_keys.insert(to.clone(), keys);
        }
        if let Some(attrs) = saved_attrs.remove(from) {
            saved_attrs.insert(to.clone(), attrs);
        }
    }

    renames
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
) -> Vec<AppError> {
    match identifier::compute_anonymous_identifiers(
        resources,
        providers,
        ctx.schemas(),
        &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        &|name| identity_attributes_for_provider(ctx, name),
    ) {
        Ok(()) => Vec::new(),
        Err(msg) => vec![AppError::Config(msg)],
    }
}

/// Encapsulates the plan-time normalization pipeline.
///
/// Ensures the correct ordering of normalization steps that must run
/// between reference resolution and plan creation:
///
/// 1. `normalize_desired` — resolve DSL enum identifiers
/// 2. `normalize_state` — convert raw API values to match DSL format
/// 3. `merge_default_tags` — add provider-level default tags (must run after normalize_desired)
/// 4. `resolve_enum_aliases` — convert to canonical AWS values in both resources and states
pub struct PlanPreprocessor<'a> {
    normalizer: &'a dyn ProviderNormalizer,
    ctx: &'a WiringContext,
}

impl<'a> PlanPreprocessor<'a> {
    pub fn new(normalizer: &'a dyn ProviderNormalizer, ctx: &'a WiringContext) -> Self {
        Self { normalizer, ctx }
    }

    /// Run the full normalization pipeline on desired resources and current states.
    ///
    /// Call after `resolve_refs_with_state_and_remote()` and before `create_plan()`.
    pub fn prepare(
        &self,
        resources: &mut [Resource],
        current_states: &mut HashMap<ResourceId, State>,
        provider_configs: &[ProviderConfig],
    ) {
        self.normalizer.normalize_desired(resources);
        self.normalizer.normalize_state(current_states);
        let schemas = self.ctx.schemas();
        for config in provider_configs {
            if !config.default_tags.is_empty() {
                self.normalizer
                    .merge_default_tags(resources, &config.default_tags, schemas);
            }
        }
        resolve_enum_aliases_with_ctx(self.ctx, resources);
        resolve_enum_aliases_in_states(self.ctx, current_states);
    }
}

/// Run provider-specific normalization on desired resources.
///
/// Creates normalizers from all registered provider factories and applies
/// `normalize_desired()` to the resources. This resolves enum identifiers
/// (e.g., bare enum identifiers -> namespaced enum strings) without requiring
/// actual provider instances or network access.
pub fn normalize_desired_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("failed to build tokio runtime for normalize_desired");
    let mut router = ProviderRouter::new();
    for factory in ctx.factories() {
        let attrs = HashMap::new();
        router.add_normalizer(rt.block_on(factory.create_normalizer(&attrs)));
    }
    router.normalize_desired(resources);
}

/// Normalize enum values in current states to match DSL format.
///
/// Creates normalizers from all registered provider factories and applies
/// `normalize_state()` to the current states. This converts raw AWS enum
/// values (e.g., `"ap-northeast-1a"`) to the same DSL format that
/// `normalize_desired` produces, preventing false diffs.
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
        router.add_normalizer(rt.block_on(factory.create_normalizer(&attrs)));
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
            if let Some(canonical) = factory.get_enum_alias_reverse(resource_type, attr_name, raw) {
                *s = canonical;
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
) -> Vec<AppError> {
    lift_validation_result(validation::validate_provider_config(
        parsed,
        ctx.factories(),
    ))
}

pub fn validate_module_calls(
    parsed: &ParsedFile,
    base_dir: &Path,
    config: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    let mut imported_modules = HashMap::new();
    for import in &parsed.imports {
        let module_path = base_dir.join(&import.path);
        if let Some(module_parsed) = module_resolver::load_module(&module_path) {
            imported_modules.insert(import.alias.clone(), module_parsed.arguments);
        }
    }

    lift_validation_result(validation::validate_module_calls(
        &parsed.module_calls,
        &imported_modules,
        config,
    ))
}

pub fn validate_module_attribute_param_types(
    ctx: &WiringContext,
    parsed: &ParsedFile,
    base_dir: &Path,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    for import in &parsed.imports {
        let module_path = base_dir.join(&import.path);
        let Some(module_parsed) = module_resolver::load_module(&module_path) else {
            continue;
        };
        if module_parsed.attribute_params.is_empty() {
            continue;
        }
        if let Err(joined) = validation::validate_attribute_param_ref_types(
            &module_parsed.attribute_params,
            &module_parsed.resources,
            ctx.schemas(),
            &|r| provider_mod::schema_key_for_resource(ctx.factories(), r),
        ) {
            // Preserve the module-path prefix the legacy wrapper emitted
            // so diagnostics point at which imported module failed.
            let prefix = import.path.to_string();
            errors.extend(
                joined
                    .split('\n')
                    .filter(|s| !s.is_empty())
                    .map(|s| AppError::Validation(format!("{}: {}", prefix, s))),
            );
        }
    }
    errors
}

pub async fn get_provider_with_ctx(
    ctx: &WiringContext,
    parsed: &ParsedFile,
    base_dir: &Path,
) -> ProviderRouter {
    let mut router = ProviderRouter::new();

    for provider_config in &parsed.providers {
        // If the provider has a source, load it as a WASM plugin
        if let Some(ref source) = provider_config.source {
            try_add_source_provider(&mut router, source, provider_config, base_dir).await;
            continue;
        }

        // Otherwise, look up from the dynamic factories passed to WiringContext
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&provider_config.attributes).await;
            router.add_provider(provider_config.name.clone(), provider);
            router.add_normalizer(factory.create_normalizer(&provider_config.attributes).await);
        } else if !provider_config.name.is_empty() {
            eprintln!(
                "{}",
                format!(
                    "Provider '{}' requires 'source' and 'version' attributes.",
                    provider_config.name
                )
                .red()
            );
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

async fn try_add_source_provider(
    router: &mut ProviderRouter,
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) {
    match load_source_provider(source, config, base_dir).await {
        Ok((factory, provider, name)) => {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {}, source: {})", name, region, source).cyan()
            );
            router.add_provider(name, provider);
            router.add_normalizer(factory.create_normalizer(&config.attributes).await);
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("Failed to load provider '{}': {}", config.name, e).red()
            );
        }
    }
}

async fn load_source_provider(
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) -> Result<(Box<dyn ProviderFactory>, Box<dyn Provider>, String), String> {
    let binary_path = if source.starts_with("file://") || source.starts_with("github.com/") {
        carina_provider_resolver::find_installed_provider(base_dir, config)
            .map_err(|e| format!("Provider '{}' {}", config.name, e))?
    } else {
        return Err(format!(
            "Unsupported source format: {source}. Use file:// for local binaries or github.com/owner/repo for remote."
        ));
    };

    if !carina_provider_resolver::is_wasm_provider(&binary_path) {
        return Err(format!(
            "Provider '{}': native binaries are no longer supported. Use a .wasm component instead.",
            config.name
        ));
    }

    let factory: Box<dyn ProviderFactory> = Box::new(
        carina_plugin_host::WasmProviderFactory::new(binary_path.clone())
            .await
            .map_err(|e| format!("Failed to load WASM provider: {e}"))?,
    );
    let name = factory.name().to_string();

    factory
        .validate_config(&config.attributes)
        .map_err(|e| format!("Config validation failed: {e}"))?;

    let provider = factory.create_provider(&config.attributes).await;
    Ok((factory, provider, name))
}

pub async fn create_providers_from_configs(
    configs: &[ProviderConfig],
    base_dir: &Path,
) -> ProviderRouter {
    let (factories, _) = build_factories_from_providers(configs, base_dir);
    let ctx = WiringContext::new(factories);
    let mut router = ProviderRouter::new();

    for config in configs {
        // If the provider has a source, load it as a WASM plugin
        if let Some(ref source) = config.source {
            try_add_source_provider(&mut router, source, config, base_dir).await;
            continue;
        }

        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &config.name) {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&config.attributes).await;
            router.add_provider(config.name.clone(), provider);
            router.add_normalizer(factory.create_normalizer(&config.attributes).await);
        }
    }

    if router.is_empty() {
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    router
}

/// Create a plan from parsed configuration (without upstream state bindings).
///
/// This is a convenience wrapper around `create_plan_from_parsed_with_upstream`
/// for callers that don't use upstream_state blocks.
#[allow(dead_code)]
pub async fn create_plan_from_parsed(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
    refresh: bool,
    base_dir: &Path,
) -> Result<PlanContext, AppError> {
    create_plan_from_parsed_with_upstream(parsed, state_file, refresh, &HashMap::new(), base_dir)
        .await
}

pub async fn create_plan_from_parsed_with_upstream(
    parsed: &ParsedFile,
    state_file: &Option<StateFile>,
    refresh: bool,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
    base_dir: &Path,
) -> Result<PlanContext, AppError> {
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir);
    let ctx = WiringContext::new(factories);
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).map_err(AppError::Validation)?;

    // Select appropriate Provider based on configuration
    let provider = get_provider_with_ctx(&ctx, parsed, base_dir).await;

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();

    // Build state-file-derived maps up front so anonymous → let-bound
    // rename transfer (#1685) can run between refresh phases 1 and 2.
    // These maps only depend on `state_file`, not on refresh output.
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();
    let mut prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();
    // `moved_pairs` accumulates explicit `moved` block transfers and
    // detected anonymous → let-bound renames. Populated inside the
    // refresh block so the later plan-building code sees them.
    let mut moved_pairs: Vec<(ResourceId, ResourceId)> = Vec::new();

    if refresh {
        RefreshProgress::start_header();
        let multi = refresh_multi_progress();

        // Read states for all resources concurrently using identifier from state.
        // In identifier-based approach, if there's no identifier in state, the resource doesn't exist.
        // Skip virtual resources (module attribute containers) — they have no infrastructure.
        let provider_ref = &provider;
        // Pre-build a map of dependency_bindings from the state file so we can
        // restore them after refresh. Provider.read() returns fresh attributes but
        // doesn't know about dependency_bindings (carina-only metadata).
        let saved_dep_bindings: HashMap<ResourceId, Vec<String>> = state_file
            .as_ref()
            .map(|sf| {
                sorted_resources
                    .iter()
                    .filter_map(|r| {
                        let rs =
                            sf.find_resource(&r.id.provider, &r.id.resource_type, &r.id.name)?;
                        if rs.dependency_bindings.is_empty() {
                            None
                        } else {
                            Some((r.id.clone(), rs.dependency_bindings.clone()))
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Refresh in two phases so data sources can see concrete values
        // from their dependencies (#1683). Phase 1: managed resources in
        // parallel. Phase 2: data sources whose input attributes have
        // been resolved against phase 1's `current_states`.
        let phase1_results: Vec<Result<(ResourceId, State), AppError>> = stream::iter(
            sorted_resources
                .iter()
                .filter(|r| !r.is_virtual() && !r.is_data_source()),
        )
        .map(|resource| {
            let progress = RefreshProgress::begin_multi(&multi, &resource.id);
            let identifier = state_file
                .as_ref()
                .and_then(|sf| sf.get_identifier_for_resource(resource));
            let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
            async move {
                let mut state = read_with_retry(provider_ref, &resource.id, identifier.as_deref())
                    .await
                    .map_err(AppError::Provider)?;
                // Restore dependency_bindings from state file (#1565).
                if let Some(deps) = dep_bindings {
                    state.dependency_bindings = deps;
                }
                progress.finish();
                Ok((resource.id.clone(), state))
            }
        })
        .buffer_unordered(5)
        .collect()
        .await;
        for result in phase1_results {
            let (id, state) = result?;
            current_states.insert(id, state);
        }

        // Refresh orphaned resources (#844). These are tracked in state
        // but removed from the .crn config — they're looked up by their
        // *old* name, which includes the pre-rename anonymous name of a
        // let-bound resource. Must run before the rename transfer below
        // so that transfer has the old-name state entries to move.
        if let Some(sf) = state_file.as_ref() {
            let desired_ids: HashSet<ResourceId> =
                sorted_resources.iter().map(|r| r.id.clone()).collect();
            let orphan_states: Vec<(ResourceId, State)> =
                sf.build_orphan_states(&desired_ids).into_iter().collect();
            let orphan_results: Vec<Result<(ResourceId, State), AppError>> =
                stream::iter(orphan_states)
                    .map(|(id, state)| {
                        let progress = RefreshProgress::begin_multi(&multi, &id);
                        // Preserve _binding and dependency_bindings from state file
                        // so orphan Delete effects retain metadata after refresh (#1548, #1565).
                        let binding = state.attributes.get("_binding").cloned();
                        let dep_bindings = state.dependency_bindings.clone();
                        async move {
                            let mut refreshed =
                                read_with_retry(provider_ref, &id, state.identifier.as_deref())
                                    .await
                                    .map_err(AppError::Provider)?;
                            if let Some(b) = binding {
                                refreshed.attributes.insert("_binding".to_string(), b);
                            }
                            if !dep_bindings.is_empty() {
                                refreshed.dependency_bindings = dep_bindings;
                            }
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

        // Hydrate now — before phase 2 resolves data source refs — so
        // any attributes the provider's read() didn't return are
        // available when building the binding map (#1685).
        provider.hydrate_read_state(&mut current_states, &saved_attrs);

        // Transfer state for explicit `moved` blocks and anonymous →
        // let-bound renames (#1685). Must run before phase 2 so the ref
        // resolver sees state entries under their *new* binding name.
        // Detection operates on `sorted_resources` (pre-resolved), which
        // is sufficient for the common case of literal create-only
        // attributes; resources with ResourceRef create-only values are
        // an orthogonal edge case that pre-dates this fix.
        moved_pairs.extend(materialize_moved_states(
            &mut current_states,
            &mut prev_desired_keys,
            &mut saved_attrs,
            &parsed.state_blocks,
            state_file,
        ));
        moved_pairs.extend(apply_anonymous_to_named_renames(
            &ctx,
            &sorted_resources,
            &parsed.providers,
            &mut current_states,
            &mut prev_desired_keys,
            &mut saved_attrs,
            state_file,
        ));

        // Phase 2: resolve data source refs against the consolidated
        // `current_states`, then refresh each via `read_data_source`.
        let resolved_data_sources = resolve_data_source_refs_for_refresh(
            &sorted_resources,
            &current_states,
            remote_bindings,
        )?;
        let phase2_results: Vec<Result<(ResourceId, State), AppError>> =
            stream::iter(resolved_data_sources.iter())
                .map(|resource| {
                    let progress = RefreshProgress::begin_multi(&multi, &resource.id);
                    let dep_bindings = saved_dep_bindings.get(&resource.id).cloned();
                    async move {
                        let mut state = read_data_source_with_retry(provider_ref, resource)
                            .await
                            .map_err(AppError::Provider)?;
                        if let Some(deps) = dep_bindings {
                            state.dependency_bindings = deps;
                        }
                        progress.finish();
                        Ok((resource.id.clone(), state))
                    }
                })
                .buffer_unordered(5)
                .collect()
                .await;
        for result in phase2_results {
            let (id, state) = result?;
            current_states.insert(id, state);
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

            // Transfer state for moved blocks and anonymous → let-bound
            // renames so the later `resolve_refs_with_state_and_remote`
            // call (and data-source-input resolution) sees entries
            // under their current binding names (#1685).
            provider.hydrate_read_state(&mut current_states, &saved_attrs);
            moved_pairs.extend(materialize_moved_states(
                &mut current_states,
                &mut prev_desired_keys,
                &mut saved_attrs,
                &parsed.state_blocks,
                state_file,
            ));
            moved_pairs.extend(apply_anonymous_to_named_renames(
                &ctx,
                &sorted_resources,
                &parsed.providers,
                &mut current_states,
                &mut prev_desired_keys,
                &mut saved_attrs,
                state_file,
            ));
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

    // Resolve ResourceRef values and enum identifiers using AWS state
    let mut resources = sorted_resources.clone();
    resolve_refs_with_state_and_remote(&mut resources, &current_states, remote_bindings)?;

    // Run the normalization pipeline: normalize_desired → normalize_state →
    // merge_default_tags → resolve_enum_aliases (order matters).
    let preprocessor = PlanPreprocessor::new(&provider, &ctx);
    preprocessor.prepare(&mut resources, &mut current_states, &parsed.providers);

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
    add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        state_file,
        &moved_pairs,
        ctx.schemas(),
    );

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
    schemas: &HashMap<String, ResourceSchema>,
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
                // Try exact match first; fall back to matching against anonymous
                // resources via the schema's name_attribute. This lets users write
                // `to = awscc.s3.bucket 'carina-rs-state'` without needing the
                // auto-generated hash name.
                let effective_to = resolve_import_target(to, plan, state_file, schemas);

                // Skip if resource already exists in state
                let already_in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(
                        &effective_to.provider,
                        &effective_to.resource_type,
                        &effective_to.name,
                    )
                    .is_some()
                });
                if !already_in_state {
                    suppress_create.insert(effective_to.clone());
                    new_effects.push(Effect::Import {
                        id: effective_to,
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

/// Resolve an import block's `to` address to a matching resource in the plan or state.
///
/// Tries exact match first (by provider, resource_type, name). If no exact match
/// exists, falls back to matching `to.name` against the `name_attribute` values of:
/// 1. Anonymous resources in the plan's Create effects (pre-apply case)
/// 2. Resources in the state file (already-imported case)
///
/// This lets users write `to = awscc.s3.bucket 'carina-rs-state'` without needing
/// the auto-generated hash name, matching against `bucket_name = 'carina-rs-state'`.
fn resolve_import_target(
    to: &ResourceId,
    plan: &Plan,
    state_file: &Option<StateFile>,
    schemas: &HashMap<String, ResourceSchema>,
) -> ResourceId {
    let name_attr = schemas
        .get(&to.display_type())
        .and_then(|s| s.name_attribute.as_deref());

    // Single pass: prefer exact id match, otherwise remember the first name_attribute match.
    let mut fallback_id: Option<ResourceId> = None;
    for effect in plan.effects() {
        let Effect::Create(resource) = effect else {
            continue;
        };
        if resource.id == *to {
            return to.clone();
        }
        if fallback_id.is_some() {
            continue;
        }
        if resource.id.provider != to.provider || resource.id.resource_type != to.resource_type {
            continue;
        }
        if let Some(attr) = name_attr
            && let Some(Value::String(s)) = resource.get_attr(attr)
            && s == &to.name
        {
            fallback_id = Some(resource.id.clone());
        }
    }
    if let Some(id) = fallback_id {
        return id;
    }

    // Fallback: match by name_attribute value in state file (already-imported case)
    if let Some(attr) = name_attr
        && let Some(sf) = state_file.as_ref()
    {
        for rs in sf.resources_by_type(&to.provider, &to.resource_type) {
            if let Some(serde_json::Value::String(s)) = rs.attributes.get(attr)
                && s == &to.name
            {
                return ResourceId::with_provider(&rs.provider, &rs.resource_type, &rs.name);
            }
        }
    }

    to.clone()
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

/// Read a data source resource via the provider with retry on throttling errors.
///
/// Same backoff policy as [`read_with_retry`] but uses [`Provider::read_data_source`]
/// so the provider receives the full [`Resource`] (including user-supplied input
/// attributes) rather than just the identifier.
pub async fn read_data_source_with_retry(
    provider: &dyn Provider,
    resource: &Resource,
) -> Result<State, ProviderError> {
    let max_retries = 3;
    for attempt in 0..=max_retries {
        match provider.read_data_source(resource).await {
            Ok(state) => return Ok(state),
            Err(e) if attempt < max_retries && is_throttling_error(&e) => {
                let delay = Duration::from_secs(1 << attempt); // 1s, 2s, 4s
                eprintln!(
                    "  Throttled reading {}, retrying in {}s...",
                    resource.id,
                    delay.as_secs()
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Resolve `ResourceRef` values in data source input attributes against
/// already-refreshed `current_states`, returning the data sources ready
/// to pass to `read_data_source_with_retry` (#1683).
///
/// The full `sorted_resources` slice must be passed (not a pre-filtered
/// data-sources-only slice) because `resolve_refs_with_state_and_remote`
/// builds its binding map from every resource with a `binding`, including
/// managed ones that data sources reference.
pub(crate) fn resolve_data_source_refs_for_refresh(
    sorted_resources: &[Resource],
    current_states: &HashMap<ResourceId, State>,
    remote_bindings: &HashMap<String, HashMap<String, Value>>,
) -> Result<Vec<Resource>, AppError> {
    let mut resolved = sorted_resources.to_vec();
    resolve_refs_with_state_and_remote(&mut resolved, current_states, remote_bindings)
        .map_err(AppError::Validation)?;
    Ok(resolved
        .into_iter()
        .filter(|r| !r.is_virtual() && r.is_data_source())
        .collect())
}

/// Convenience wrappers for tests. Each creates a fresh `WiringContext` internally,
/// which is acceptable in test code where the overhead is negligible.
#[cfg(test)]
pub fn validate_resources(resources: &[Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![]);
    let parsed = ParsedFile {
        resources: resources.to_vec(),
        ..ParsedFile::default()
    };
    errors_to_legacy_result(validate_resources_with_ctx(&ctx, &parsed))
}

#[cfg(test)]
pub fn resolve_names(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![]);
    errors_to_legacy_result(resolve_names_with_ctx(&ctx, resources))
}

#[cfg(test)]
pub fn resolve_attr_prefixes(resources: &mut [Resource]) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![]);
    errors_to_legacy_result(resolve_attr_prefixes_with_ctx(&ctx, resources))
}

#[cfg(test)]
pub fn compute_anonymous_identifiers(
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Result<(), AppError> {
    let ctx = WiringContext::new(vec![]);
    let errors = compute_anonymous_identifiers_with_ctx(&ctx, resources, providers);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(crate::commands::collapse_errors(errors))
    }
}

/// Test-only adapter: collapse a `Vec<AppError>` back into the
/// `Result<(), AppError>` shape pre-#2105 callers expect. Always
/// joins as `AppError::Validation` — the three wrappers that use it
/// only return validation-kind errors.
#[cfg(test)]
fn errors_to_legacy_result(errors: Vec<AppError>) -> Result<(), AppError> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Validation(
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ))
    }
}

#[cfg(test)]
pub fn resolve_enum_aliases(resources: &mut [Resource]) {
    let ctx = WiringContext::new(vec![]);
    resolve_enum_aliases_with_ctx(&ctx, resources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires provider binary for enum alias resolution"]
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
    #[ignore = "requires provider binary for enum alias resolution"]
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
    #[ignore = "requires provider binary for enum alias resolution"]
    fn test_resolve_enum_aliases_in_states() {
        // Current states should also have aliases resolved
        let ctx = WiringContext::new(vec![]);
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
    #[ignore = "requires provider binary for enum alias resolution"]
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
    #[ignore = "requires provider binary for state normalization"]
    fn test_normalize_state_prevents_false_enum_diff() {
        use carina_core::differ::create_plan;
        use carina_core::resource::LifecycleConfig;
        use carina_core::schema::ResourceSchema;

        let ctx = WiringContext::new(vec![]);

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
    #[ignore = "requires provider binary for default tags merging"]
    fn test_merge_default_tags_prevents_false_diff() {
        use carina_core::differ::create_plan;
        use carina_core::resource::LifecycleConfig;
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

        // Build a minimal schema that has a "tags" attribute.
        // merge_default_tags checks for the presence of "tags" in the schema.
        let schema = ResourceSchema::new("awscc.s3.bucket").attribute(AttributeSchema::new(
            "tags",
            AttributeType::map(AttributeType::String),
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
        let ctx = WiringContext::new(vec![]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build tokio runtime");
        let mut router = ProviderRouter::new();
        for factory in ctx.factories() {
            let attrs = HashMap::new();
            router.add_normalizer(rt.block_on(factory.create_normalizer(&attrs)));
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

    #[test]
    fn import_fallback_matches_anonymous_resource_by_name_attribute() {
        use carina_core::effect::Effect;
        use carina_core::plan::Plan;
        use carina_core::resource::{Resource, ResourceId, Value};
        use carina_core::schema::ResourceSchema;

        // Schema with name_attribute = "bucket_name"
        let bucket_schema =
            ResourceSchema::new("awscc.s3.bucket").with_name_attribute("bucket_name");
        let mut schemas = HashMap::new();
        schemas.insert("awscc.s3.bucket".to_string(), bucket_schema);

        // Anonymous resource with hash name but bucket_name = "carina-rs-state"
        let mut resource = Resource::with_provider("awscc", "s3.bucket", "s3_bucket_1d43a664");
        resource.set_attr(
            "bucket_name".to_string(),
            Value::String("carina-rs-state".to_string()),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(resource));

        // Import block with the logical name (not the hash)
        let state_blocks = vec![StateBlock::Import {
            to: ResourceId::with_provider("awscc", "s3.bucket", "carina-rs-state"),
            id: "carina-rs-state".to_string(),
        }];

        add_state_block_effects(&mut plan, &state_blocks, &None, &[], &schemas);

        // Expect only an Import effect (no Create) targeting the anonymous hash name
        let effects = plan.effects();
        assert_eq!(
            effects.len(),
            1,
            "Expected only Import effect, got {effects:?}"
        );
        match &effects[0] {
            Effect::Import { id, identifier } => {
                assert_eq!(
                    id.name, "s3_bucket_1d43a664",
                    "Import should target the anonymous hash name"
                );
                assert_eq!(identifier, "carina-rs-state");
            }
            other => panic!("Expected Import effect, got {other:?}"),
        }
    }

    #[test]
    fn import_fallback_skips_when_already_in_state_by_name_attribute() {
        use carina_core::plan::Plan;
        use carina_core::resource::ResourceId;
        use carina_core::schema::ResourceSchema;
        use carina_state::state::{ResourceState, StateFile};

        let bucket_schema =
            ResourceSchema::new("awscc.s3.bucket").with_name_attribute("bucket_name");
        let mut schemas = HashMap::new();
        schemas.insert("awscc.s3.bucket".to_string(), bucket_schema);

        // State has the resource under its anonymous hash name
        let mut state_file = StateFile::new();
        let mut rs = ResourceState::new("s3.bucket", "s3_bucket_1d43a664", "awscc");
        rs.attributes.insert(
            "bucket_name".to_string(),
            serde_json::Value::String("carina-rs-state".to_string()),
        );
        state_file.resources.push(rs);

        let mut plan = Plan::new();
        let state_blocks = vec![StateBlock::Import {
            to: ResourceId::with_provider("awscc", "s3.bucket", "carina-rs-state"),
            id: "carina-rs-state".to_string(),
        }];

        add_state_block_effects(&mut plan, &state_blocks, &Some(state_file), &[], &schemas);

        // Already in state (via fallback match) — no Import effect should be emitted
        assert_eq!(
            plan.effects().len(),
            0,
            "Import should be skipped when fallback-matched resource is already in state"
        );
    }

    /// Regression test for carina#1683: data source input attributes that
    /// reference another resource must be resolved against current state
    /// *before* being passed to `read_data_source_with_retry`. Without
    /// resolution the provider receives a debug-formatted `ResourceRef`
    /// string and ships it to the remote API as a literal.
    #[test]
    fn resolve_data_source_refs_replaces_resource_ref_with_concrete_value() {
        use carina_core::resource::{AccessPath, Expr, PathSegment, ResourceKind};

        let identity_store_id = "d-9067c29a4b";

        // Managed resource with a binding — phase 1 would have refreshed it.
        let mut sso = Resource::with_provider("awscc", "sso.instance", "carina-rs");
        sso.binding = Some("sso".to_string());

        // Data source referencing `sso.identity_store_id`.
        let mut mizzy = Resource::with_provider("aws", "identitystore.user", "mizzy");
        mizzy.kind = ResourceKind::DataSource;
        mizzy.attributes.insert(
            "identity_store_id".to_string(),
            Expr(Value::ResourceRef {
                path: AccessPath(vec![
                    PathSegment::Field("sso".into()),
                    PathSegment::Field("identity_store_id".into()),
                ]),
            }),
        );
        mizzy.attributes.insert(
            "user_name".to_string(),
            Expr(Value::String("gosukenator@gmail.com".into())),
        );

        // current_states after phase 1: sso has been refreshed and its
        // state carries the concrete identity_store_id.
        let mut current_states: HashMap<ResourceId, State> = HashMap::new();
        let sso_state = State::existing(
            sso.id.clone(),
            HashMap::from([(
                "identity_store_id".to_string(),
                Value::String(identity_store_id.into()),
            )]),
        );
        current_states.insert(sso.id.clone(), sso_state);

        let resolved =
            resolve_data_source_refs_for_refresh(&[sso, mizzy], &current_states, &HashMap::new())
                .expect("resolution should succeed");

        assert_eq!(resolved.len(), 1, "only the data source should be returned");
        let resolved_mizzy = &resolved[0];
        assert_eq!(
            resolved_mizzy.get_attr("identity_store_id"),
            Some(&Value::String(identity_store_id.into())),
            "identity_store_id should be resolved to the concrete state value, \
             not a ResourceRef"
        );
        assert_eq!(
            resolved_mizzy.get_attr("user_name"),
            Some(&Value::String("gosukenator@gmail.com".into())),
            "literal inputs should pass through untouched"
        );
    }

    // Two resources with unknown types must surface as two distinct
    // `AppError::Validation` entries instead of one joined string, so
    // the driver can accumulate diagnostics across validators.
    #[test]
    fn validate_resources_with_ctx_returns_each_error_as_app_error() {
        let ctx = WiringContext::new(vec![]);

        // Empty provider string sidesteps the "unknown provider, skip"
        // escape hatch (`known_providers` is empty), so each bad resource
        // produces its own "Unknown resource type" entry.
        let r1 = Resource::new("foo.nothing", "first");
        let r2 = Resource::new("bar.nothing", "second");
        let parsed = ParsedFile {
            resources: vec![r1, r2],
            ..ParsedFile::default()
        };

        let errors = validate_resources_with_ctx(&ctx, &parsed);
        assert_eq!(errors.len(), 2, "got {errors:?}");
        for err in &errors {
            assert!(matches!(err, AppError::Validation(_)), "got {err:?}");
        }
    }

    // Smoke test for the dependency-chain wrappers: empty input
    // exercises each wrapper and pins the `Vec<AppError>` return
    // type. A regression back to `Result` fails to compile here.
    #[test]
    fn dependency_chain_wrappers_return_vec_app_error() {
        let ctx = WiringContext::new(vec![]);
        let mut resources: Vec<Resource> = Vec::new();
        let providers: Vec<ProviderConfig> = Vec::new();

        let errors = resolve_names_with_ctx(&ctx, &mut resources);
        assert!(errors.is_empty(), "resolve_names: got {errors:?}");

        let errors = resolve_attr_prefixes_with_ctx(&ctx, &mut resources);
        assert!(errors.is_empty(), "resolve_attr_prefixes: got {errors:?}");

        let errors = compute_anonymous_identifiers_with_ctx(&ctx, &mut resources, &providers);
        assert!(
            errors.is_empty(),
            "compute_anonymous_identifiers: got {errors:?}",
        );
    }

    // Smoke test for the module/provider wrappers: empty input exercises
    // each wrapper and pins the `Vec<AppError>` return type. A regression
    // back to `Result` fails to compile here.
    #[test]
    fn module_and_provider_wrappers_return_vec_app_error() {
        let ctx = WiringContext::new(vec![]);
        let parsed = ParsedFile::default();
        let base_dir = std::path::Path::new("/tmp/nonexistent-carina-pr3-test");
        let provider_ctx = carina_core::parser::ProviderContext::default();

        let errors = validate_provider_region_with_ctx(&ctx, &parsed);
        assert!(errors.is_empty(), "provider_region: got {errors:?}");

        let errors = validate_module_calls(&parsed, base_dir, &provider_ctx);
        assert!(errors.is_empty(), "module_calls: got {errors:?}");

        let errors = validate_module_attribute_param_types(&ctx, &parsed, base_dir);
        assert!(
            errors.is_empty(),
            "module_attribute_param_types: got {errors:?}",
        );
    }
}
