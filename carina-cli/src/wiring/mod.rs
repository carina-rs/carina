use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

#[cfg(test)]
use indexmap::IndexMap;
use std::sync::Arc;
use std::time::Duration;

use colored::Colorize;
use futures::stream::{self, StreamExt};

use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::effect::Effect;
use carina_core::identifier::{self, AnonymousIdStateInfo, PrefixStateInfo};
use carina_core::module_resolver;
use carina_core::parser::{ProviderConfig, StateBlock};
use carina_core::plan::Plan;
use carina_core::provider::{
    self as provider_mod, Provider, ProviderError, ProviderFactory, ProviderNormalizer,
    ProviderRouter,
};
use carina_core::resolver::{resolve_refs_for_plan, resolve_refs_with_state_and_remote};
use carina_core::resource::{Resource, ResourceId, State, Value, contains_resource_ref};
use carina_core::schema::{SchemaRegistry, resolve_block_names};
use carina_core::utils;
use carina_core::validation;
use carina_provider_mock::MockProvider;
use carina_state::StateFile;

use crate::commands::shared::progress::{RefreshProgress, refresh_multi_progress};
use crate::error::AppError;

/// Result of creating a plan, with context needed for saving
pub struct PlanContext {
    pub plan: Plan,
    pub sorted_resources: Vec<Resource>,
    pub current_states: HashMap<ResourceId, State>,
    /// Maps moved-to resource IDs to their original (moved-from) IDs.
    /// Used by display to show "(moved from: ...)" annotations on Update/Replace effects.
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    /// Snapshot of `upstream_state` bindings as resolved at plan time.
    /// Persisted to the plan file (#2303) so apply-from-plan can verify
    /// the upstream values have not drifted before re-using them for
    /// cascade re-resolution. Empty when the configuration declares no
    /// `upstream_state` blocks.
    pub upstream_snapshot: HashMap<String, HashMap<String, carina_core::resource::Value>>,
}

/// Cached provider factories and schemas, constructed once per CLI invocation.
///
/// Instead of calling `provider_factories()` and `get_schemas()` at each call
/// site (which rebuilds the full schema set every time), create a single
/// `WiringContext` and pass it through the command execution path.
pub struct WiringContext {
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    schemas: SchemaRegistry,
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

    pub fn schemas(&self) -> &SchemaRegistry {
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

pub fn validate_resources_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    provider_context: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    let known_providers: HashSet<String> = ctx
        .factories()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    lift_validation_result(validation::validate_resources(
        parsed,
        ctx.schemas(),
        &known_providers,
        provider_context,
    ))
}

pub fn validate_resource_ref_types_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    argument_names: &HashSet<String>,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_resource_ref_types(
        parsed,
        ctx.schemas(),
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
    ))
}

/// Reject any resolved value that still carries
/// `Value::Unknown(UnknownReason::EmptyInterpolation)`. The parser
/// accepts mid-edit `${}` to keep the AST intact (#2480) and the LSP
/// surfaces it as a per-location warning, but `carina validate` /
/// `plan` / `apply` must refuse to proceed — letting the marker flow
/// to a provider would render the literal text `${}` (or worse, an
/// empty substitution) into a real API call. See #2487.
pub fn validate_no_empty_interpolations<E>(parsed: &carina_core::parser::File<E>) -> Vec<AppError>
where
    E: carina_core::parser::ExportParamLike,
{
    let mut errors = Vec::new();
    for resource in &parsed.resources {
        for (attr_name, value) in &resource.attributes {
            if value_contains_empty_interpolation(value) {
                errors.push(AppError::Validation(format!(
                    "{}: attribute `{}`: empty interpolation `${{}}` — fill in the expression or remove it",
                    resource.id, attr_name
                )));
            }
        }
    }
    for export in &parsed.export_params {
        if let Some(value) = export.value()
            && value_contains_empty_interpolation(value)
        {
            errors.push(AppError::Validation(format!(
                "exports `{}`: empty interpolation `${{}}` — fill in the expression or remove it",
                export.name()
            )));
        }
    }
    for param in &parsed.attribute_params {
        if let Some(value) = &param.value
            && value_contains_empty_interpolation(value)
        {
            errors.push(AppError::Validation(format!(
                "attributes `{}` default: empty interpolation `${{}}` — fill in the expression or remove it",
                param.name
            )));
        }
    }
    errors
}

/// Recursively walk a `Value` tree looking for any
/// `Value::Unknown(UnknownReason::EmptyInterpolation)`. Returns `true`
/// when one is found at any depth — inside lists, maps, secrets,
/// function-call arguments, or as the `Expr` segment of an
/// `Interpolation`. Mirrors the variant coverage of the sibling
/// `value_contains_unknown` to keep them in lockstep when new `Value`
/// variants land.
fn value_contains_empty_interpolation(value: &Value) -> bool {
    use carina_core::resource::{InterpolationPart, UnknownReason};
    match value {
        Value::Unknown(UnknownReason::EmptyInterpolation) => true,
        Value::Interpolation(parts) => parts.iter().any(|p| match p {
            InterpolationPart::Expr(v) => value_contains_empty_interpolation(v),
            InterpolationPart::Literal(_) => false,
        }),
        Value::List(items) => items.iter().any(value_contains_empty_interpolation),
        Value::Map(entries) => entries.values().any(value_contains_empty_interpolation),
        Value::Secret(inner) => value_contains_empty_interpolation(inner),
        Value::FunctionCall { args, .. } => args.iter().any(value_contains_empty_interpolation),
        _ => false,
    }
}

/// Resolve block name aliases and attribute prefixes in one step.
pub fn resolve_names_with_ctx(ctx: &WiringContext, resources: &mut [Resource]) -> Vec<AppError> {
    let mut errors = lift_validation_result(resolve_block_names(resources, ctx.schemas()));
    errors.extend(resolve_attr_prefixes_with_ctx(ctx, resources));
    errors
}

pub fn resolve_attr_prefixes_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
) -> Vec<AppError> {
    lift_validation_result(identifier::resolve_attr_prefixes(resources, ctx.schemas()))
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
        &|provider, resource_type| {
            let create_only_attrs = ctx
                .schemas()
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Managed,
                )
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
    state_file: &mut StateFile,
) {
    let renames = identifier::reconcile_anonymous_identifiers(
        resources,
        ctx.schemas(),
        &|provider, resource_type| {
            let create_only_attrs = ctx
                .schemas()
                .get(
                    provider,
                    resource_type,
                    carina_core::schema::SchemaKind::Managed,
                )
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

    apply_provider_prefix_renames(&renames, state_file);
}

/// Re-key state entries when `reconcile_anonymous_identifiers` produced rename
/// pairs (anonymous → anonymous due to identifier-format upgrade).
///
/// For each `(old_name, new_name)` pair, find the matching `ResourceState`
/// in `state_file.resources` and overwrite its `name` field. Downstream maps
/// (`build_saved_attrs`, `build_desired_keys`, `build_lifecycles`) then key
/// off the new name, so the differ sees the resource under its updated
/// identifier instead of an orphan-delete + create pair.
pub fn apply_provider_prefix_renames(renames: &[(String, String)], state_file: &mut StateFile) {
    if renames.is_empty() {
        return;
    }
    let by_old: HashMap<&str, &str> = renames
        .iter()
        .map(|(old, new)| (old.as_str(), new.as_str()))
        .collect();
    for sr in &mut state_file.resources {
        if let Some(new_name) = by_old.get(sr.name.as_str()) {
            sr.name = new_name.to_string();
        }
    }
}

pub fn compute_anonymous_identifiers_with_ctx(
    ctx: &WiringContext,
    resources: &mut [Resource],
    providers: &[ProviderConfig],
) -> Vec<AppError> {
    match identifier::compute_anonymous_identifiers(resources, providers, ctx.schemas(), &|name| {
        identity_attributes_for_provider(ctx, name)
    }) {
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
        // RFC #2371 stage 2 + #2387: strip every attribute the WASM
        // provider boundary refuses to serialize — `Value::Unknown`
        // (#2378) and `Value::ResourceRef` plus the wrappers that hide
        // a ref (`Interpolation` / `FunctionCall` / `Secret`-of-ref,
        // #2387) — before `normalize_desired` runs, then restore them
        // at their original index. After this pass, `core_to_wit_value`
        // is type-system-enforced to never see either kind.
        let stripped = strip_attributes_matching(resources, &|v| {
            value_contains_unknown(v) || contains_resource_ref(v)
        });
        // Hard assert (not debug_assert): RFC #2371 constraint b says
        // state files never carry `Value::Unknown`, and the
        // strip-and-restore design depends on it. A release-mode
        // violation would degrade silently into a WASM-boundary panic
        // far from the source — fail fast here instead.
        assert!(
            !current_states_contain_unknown(current_states),
            "Value::Unknown found in current_states — RFC #2371 constraint b violated"
        );
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
        restore_stripped_attributes(resources, stripped);
    }
}

/// One stripped attribute, retained so it can be re-inserted at its
/// original position after `normalize_desired` returns.
struct StrippedAttribute {
    insert_index: usize,
    key: String,
    value: carina_core::resource::Value,
}

/// Remove every attribute whose value matches `predicate` from each
/// resource's attribute map. Returns a per-resource record of what was
/// removed, keyed by `ResourceId`, so [`restore_stripped_attributes`]
/// can put them back at their original `IndexMap` position.
///
/// The single caller in `PlanPreprocessor::prepare` passes a unified
/// predicate that covers two kinds the WASM provider boundary refuses
/// to serialize:
///
/// - `value_contains_unknown` — `Value::Unknown` (RFC #2371 stage 2 /
///   #2377)
/// - `contains_resource_ref` — `Value::ResourceRef` plus the wrappers
///   that recursively contain one (`Interpolation` / `FunctionCall` /
///   `Secret` / nested `List` / `Map`); without this strip the
///   `core_to_wit_value` `_` debug-format fallback would silently
///   stringify the ref (#2387).
fn strip_attributes_matching(
    resources: &mut [Resource],
    predicate: &dyn Fn(&Value) -> bool,
) -> HashMap<ResourceId, Vec<StrippedAttribute>> {
    let mut out: HashMap<ResourceId, Vec<StrippedAttribute>> = HashMap::new();
    for resource in resources.iter_mut() {
        let mut stripped: Vec<StrippedAttribute> = Vec::new();
        // Collect keys to remove first so we don't mutate while iterating.
        let to_remove: Vec<(usize, String)> = resource
            .attributes
            .iter()
            .enumerate()
            .filter_map(|(i, (k, v))| {
                if predicate(v) {
                    Some((i, k.clone()))
                } else {
                    None
                }
            })
            .collect();
        // Process highest-index-first so each `shift_remove` doesn't
        // shift the indices of later removals.
        for (i, key) in to_remove.into_iter().rev() {
            if let Some(value) = resource.attributes.shift_remove(&key) {
                stripped.push(StrippedAttribute {
                    insert_index: i,
                    key,
                    value,
                });
            }
        }
        if !stripped.is_empty() {
            // Restoration order: lowest insert_index first, so each
            // `shift_insert` lands the entry at its pre-strip position
            // without disturbing entries with smaller indices.
            stripped.sort_by_key(|s| s.insert_index);
            out.insert(resource.id.clone(), stripped);
        }
    }
    out
}

/// Counterpart to [`strip_attributes_matching`]. Re-inserts each
/// stripped attribute at its original index in the resource's
/// attribute map. Attributes appended by `normalize_desired` (e.g.
/// provider-injected defaults) end up after the original entries,
/// which matches behavior pre-#2377 for non-stripped attributes.
fn restore_stripped_attributes(
    resources: &mut [Resource],
    mut stripped: HashMap<ResourceId, Vec<StrippedAttribute>>,
) {
    for resource in resources.iter_mut() {
        if let Some(entries) = stripped.remove(&resource.id) {
            for entry in entries {
                let target = entry.insert_index.min(resource.attributes.len());
                resource
                    .attributes
                    .shift_insert(target, entry.key, entry.value);
            }
        }
    }
}

fn value_contains_unknown(value: &carina_core::resource::Value) -> bool {
    use carina_core::resource::{InterpolationPart, Value};
    match value {
        Value::Unknown(_) => true,
        Value::List(items) => items.iter().any(value_contains_unknown),
        Value::Map(map) => map.values().any(value_contains_unknown),
        Value::Interpolation(parts) => parts.iter().any(|p| match p {
            InterpolationPart::Expr(v) => value_contains_unknown(v),
            _ => false,
        }),
        Value::FunctionCall { args, .. } => args.iter().any(value_contains_unknown),
        Value::Secret(inner) => value_contains_unknown(inner),
        _ => false,
    }
}

fn current_states_contain_unknown(states: &HashMap<ResourceId, State>) -> bool {
    states
        .values()
        .any(|state| state.attributes.values().any(value_contains_unknown))
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
        let attrs = indexmap::IndexMap::new();
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
        let attrs = indexmap::IndexMap::new();
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
        // `value_attrs` is `HashMap`-shaped, so the source order written
        // by the user does not survive alias resolution. Re-rendering the
        // attributes after this point can produce a different key order.
        resource.attributes = value_attrs.into_iter().collect();
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

pub fn check_unused_bindings<E: carina_core::parser::ExportParamLike>(
    parsed: &carina_core::parser::File<E>,
) -> Vec<String> {
    validation::check_unused_bindings(parsed)
}

pub fn validate_provider_region_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
) -> Vec<AppError> {
    lift_validation_result(validation::validate_provider_config(
        parsed,
        ctx.factories(),
    ))
}

pub fn validate_module_calls<E>(
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
    config: &carina_core::parser::ProviderContext,
) -> Vec<AppError> {
    let mut imported_modules = HashMap::new();
    for import in &parsed.uses {
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

pub fn validate_module_attribute_param_types<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
) -> Vec<AppError> {
    let mut errors = Vec::new();
    for import in &parsed.uses {
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

pub async fn get_provider_with_ctx<E>(
    ctx: &WiringContext,
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
) -> Result<ProviderRouter, AppError> {
    let mut router = ProviderRouter::new();

    for provider_config in &parsed.providers {
        // If the provider has a source, load it as a WASM plugin
        if let Some(ref source) = provider_config.source {
            try_add_source_provider(&mut router, source, provider_config, base_dir).await?;
            continue;
        }

        // Otherwise, look up from the dynamic factories passed to WiringContext
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory
                .create_provider(&provider_config.attributes)
                .await
                .map_err(|e| e.for_provider(provider_config.name.clone()))?;
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

    Ok(router)
}

async fn try_add_source_provider(
    router: &mut ProviderRouter,
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) -> Result<(), AppError> {
    match load_source_provider(source, config, base_dir).await {
        Ok((factory, provider, name)) => {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {}, source: {})", name, region, source).cyan()
            );
            router.add_provider(name, provider);
            router.add_normalizer(factory.create_normalizer(&config.attributes).await);
            Ok(())
        }
        Err(LoadSourceError::Provider(e)) => {
            // Provider init failure (e.g. allowed_account_ids mismatch).
            // Propagate verbatim so the CLI boundary can render it
            // structurally without leaking implementation-detail
            // wrappers like "Failed to load provider '...': ...".
            // Attach provider name so the renderer can label the
            // structured block with the right provider.
            Err(AppError::Provider(e.for_provider(config.name.clone())))
        }
        Err(LoadSourceError::Other(msg)) => {
            eprintln!(
                "{}",
                format!("Failed to load provider '{}': {}", config.name, msg).red()
            );
            Ok(())
        }
    }
}

/// Failure mode for `load_source_provider`. Distinguishing between a
/// provider init rejection and other plumbing failures lets the caller
/// surface the former as a structured error without the
/// "Failed to load provider '...': ..." wrapper that obscures the
/// real message (#2407).
enum LoadSourceError {
    /// The provider's `init` step rejected the configuration (e.g.,
    /// `allowed_account_ids` mismatch). Message is user-facing.
    Provider(ProviderError),
    /// Plumbing failure: binary not found, unsupported source scheme,
    /// invalid config, etc. Logged and skipped.
    Other(String),
}

async fn load_source_provider(
    source: &str,
    config: &ProviderConfig,
    base_dir: &Path,
) -> Result<(Box<dyn ProviderFactory>, Box<dyn Provider>, String), LoadSourceError> {
    let binary_path = if source.starts_with("file://") || source.starts_with("github.com/") {
        carina_provider_resolver::find_installed_provider(base_dir, config)
            .map_err(|e| LoadSourceError::Other(format!("Provider '{}' {}", config.name, e)))?
    } else {
        return Err(LoadSourceError::Other(format!(
            "Unsupported source format: {source}. Use file:// for local binaries or github.com/owner/repo for remote."
        )));
    };

    if !carina_provider_resolver::is_wasm_provider(&binary_path) {
        return Err(LoadSourceError::Other(format!(
            "Provider '{}': native binaries are no longer supported. Use a .wasm component instead.",
            config.name
        )));
    }

    let factory: Box<dyn ProviderFactory> = Box::new(
        carina_plugin_host::WasmProviderFactory::new(binary_path.clone())
            .await
            .map_err(|e| LoadSourceError::Other(format!("Failed to load WASM provider: {e}")))?,
    );
    let name = factory.name().to_string();

    factory
        .validate_config(&config.attributes)
        .map_err(|e| LoadSourceError::Other(format!("Config validation failed: {e}")))?;

    let provider = factory
        .create_provider(&config.attributes)
        .await
        .map_err(LoadSourceError::Provider)?;
    Ok((factory, provider, name))
}

pub async fn create_providers_from_configs(
    configs: &[ProviderConfig],
    base_dir: &Path,
) -> Result<ProviderRouter, AppError> {
    let (factories, _) = build_factories_from_providers(configs, base_dir);
    let ctx = WiringContext::new(factories);
    let mut router = ProviderRouter::new();

    for config in configs {
        // If the provider has a source, load it as a WASM plugin
        if let Some(ref source) = config.source {
            try_add_source_provider(&mut router, source, config, base_dir).await?;
            continue;
        }

        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &config.name) {
            let region = factory.extract_region(&config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory
                .create_provider(&config.attributes)
                .await
                .map_err(|e| e.for_provider(config.name.clone()))?;
            router.add_provider(config.name.clone(), provider);
            router.add_normalizer(factory.create_normalizer(&config.attributes).await);
        }
    }

    if router.is_empty() {
        println!("{}", "Using mock provider".cyan());
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    Ok(router)
}

/// Create a plan from parsed configuration (without upstream state bindings).
///
/// This is a convenience wrapper around `create_plan_from_parsed_with_upstream`
/// for callers that don't use upstream_state blocks.
#[allow(dead_code)]
pub async fn create_plan_from_parsed<E>(
    parsed: &carina_core::parser::File<E>,
    state_file: &Option<StateFile>,
    refresh: bool,
    base_dir: &Path,
) -> Result<PlanContext, AppError> {
    create_plan_from_parsed_with_upstream(parsed, state_file, refresh, &HashMap::new(), base_dir)
        .await
}

pub async fn create_plan_from_parsed_with_upstream<E>(
    parsed: &carina_core::parser::File<E>,
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
    let provider = get_provider_with_ctx(&ctx, parsed, base_dir).await?;

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
        let saved_dep_bindings: HashMap<ResourceId, BTreeSet<String>> = state_file
            .as_ref()
            .map(|sf| {
                sorted_resources
                    .iter()
                    .filter_map(|r| {
                        let rs =
                            sf.find_resource(&r.id.provider, &r.id.resource_type, r.id.name_str())?;
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
            ctx.schemas(),
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

    // Resolve ResourceRef values and enum identifiers using AWS state.
    // Plan-only: surviving upstream refs are stamped for display as
    // `(known after upstream apply: <ref>)` (#2366). `apply` calls
    // `resolve_refs_with_state_and_remote` and still errors on
    // unresolved upstream references.
    let mut resources = sorted_resources.clone();
    resolve_refs_for_plan(&mut resources, &current_states, remote_bindings)?;

    // Type-level canonicalization for `Union[String, list(String)]`
    // fields (IAM-style `string_or_list_of_strings`). Done after refs
    // resolve so concrete `String` / `List` shapes can be folded to the
    // canonical `Value::StringList` form before differ / display see
    // them. See #2481, #2511.
    carina_core::value::canonicalize_resources_with_schemas(&mut resources, ctx.schemas());
    // Same canonicalization for the actual-side state values (#2481, #2513).
    // Existing state files written before this change come back from
    // serde with the legacy `String` / `List` shape; converging both
    // sides on `StringList` lets the differ produce no diff against a
    // canonical desired side.
    carina_core::value::canonicalize_states_with_schemas(&mut current_states, ctx.schemas());

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
        upstream_snapshot: remote_bindings.clone(),
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
                sf.find_resource(&from.provider, &from.resource_type, from.name_str())
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
    registry: &SchemaRegistry,
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
                // `to = awscc.s3.Bucket 'carina-rs-state'` without needing the
                // auto-generated hash name.
                let effective_to = resolve_import_target(to, plan, state_file, registry);

                // Skip if resource already exists in state
                let already_in_state = state_file.as_ref().is_some_and(|sf| {
                    sf.find_resource(
                        &effective_to.provider,
                        &effective_to.resource_type,
                        effective_to.name_str(),
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
                    sf.find_resource(&from.provider, &from.resource_type, from.name_str())
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
/// This lets users write `to = awscc.s3.Bucket 'carina-rs-state'` without needing
/// the auto-generated hash name, matching against `bucket_name = 'carina-rs-state'`.
fn resolve_import_target(
    to: &ResourceId,
    plan: &Plan,
    state_file: &Option<StateFile>,
    registry: &SchemaRegistry,
) -> ResourceId {
    let name_attr = registry
        .get(
            &to.provider,
            &to.resource_type,
            carina_core::schema::SchemaKind::Managed,
        )
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
            && s == to.name_str()
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
                && s == to.name_str()
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
    schemas: &carina_core::schema::SchemaRegistry,
) -> Result<Vec<Resource>, AppError> {
    let mut resolved = sorted_resources.to_vec();
    resolve_refs_with_state_and_remote(&mut resolved, current_states, remote_bindings)
        .map_err(AppError::Validation)?;
    carina_core::value::canonicalize_resources_with_schemas(&mut resolved, schemas);
    Ok(resolved
        .into_iter()
        .filter(|r| !r.is_virtual() && r.is_data_source())
        .collect())
}

/// Convenience wrappers for tests. Each creates a fresh `WiringContext` internally,
/// which is acceptable in test code where the overhead is negligible.
#[cfg(test)]
pub fn validate_resources(resources: &[Resource]) -> Result<(), AppError> {
    use carina_core::parser::ParsedFile;
    let ctx = WiringContext::new(vec![]);
    let parsed = ParsedFile {
        resources: resources.to_vec(),
        ..ParsedFile::default()
    };
    errors_to_legacy_result(validate_resources_with_ctx(
        &ctx,
        &parsed,
        &carina_core::parser::ProviderContext::default(),
    ))
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
mod tests;
