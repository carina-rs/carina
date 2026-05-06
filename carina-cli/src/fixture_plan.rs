//! Build plan output from `tests/fixtures/plan_display` directories.
//!
//! Shared between the plan snapshot tests and the `plan-fixture` example
//! binary. The logic mirrors `--refresh=false` but skips provider plugin
//! loading so fixture rendering works without installed providers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::plan::Plan;
use carina_core::resolver::resolve_refs_for_plan;
use carina_core::resource::{ResourceId, State, Value};
use carina_core::schema::SchemaRegistry;
use carina_state::{StateFile, check_and_migrate};

use crate::commands::validate_and_resolve;
use crate::wiring::{
    WiringContext, normalize_desired_with_ctx, normalize_state_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
    resolve_enum_aliases_in_states, resolve_enum_aliases_with_ctx,
};

/// Fixture root path relative to the `carina-cli` crate manifest.
const FIXTURE_SUBPATH: &str = "tests/fixtures/plan_display";

/// Complete output of fixture-based plan construction.
pub struct FixturePlan {
    pub plan: Plan,
    pub current_states: HashMap<ResourceId, State>,
    pub schemas: SchemaRegistry,
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    pub deferred_for_expressions: Vec<carina_core::parser::DeferredForExpression>,
    pub export_params: Vec<carina_core::parser::InferredExportParam>,
}

/// Build a plan from a fixture directory name (e.g. "all_create"). Resolves
/// the fixture under `CARGO_MANIFEST_DIR/tests/fixtures/plan_display/<name>`.
pub fn build_plan_from_fixture_name(fixture_dir: &str) -> FixturePlan {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_path = PathBuf::from(format!(
        "{}/{}/{}",
        manifest_dir, FIXTURE_SUBPATH, fixture_dir
    ));
    build_plan_from_fixture_path(&fixture_path)
}

/// Build a plan from an absolute fixture path. Loads the configuration,
/// validates with `skip_resource_validation=true` (no provider plugin load),
/// and produces a plan equivalent to `carina plan --refresh=false`.
pub fn build_plan_from_fixture_path(fixture_path: &Path) -> FixturePlan {
    let fixture_pathbuf = fixture_path.to_path_buf();
    let state_path = fixture_path.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);

    let mut parsed = load_configuration(&fixture_pathbuf).unwrap().parsed;
    let base_dir = get_base_dir(&fixture_pathbuf);
    validate_and_resolve(&mut parsed, base_dir, true).unwrap();

    let mut state_file: Option<StateFile> = if state_path.exists() {
        let json = std::fs::read_to_string(&state_path).unwrap();
        Some(serde_json::from_str(&json).unwrap())
    } else {
        None
    };

    let wiring = WiringContext::new(vec![]);
    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.name.clone())
                    .collect()
            },
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(&wiring, &mut parsed.resources, sf);
    }

    let sorted_resources = sort_resources_by_dependencies(&parsed.resources).unwrap();

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    if let Some(sf) = state_file.as_ref() {
        for resource in &sorted_resources {
            let state = sf.build_state_for_resource(resource);
            current_states.insert(resource.id.clone(), state);
        }

        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        for (id, state) in sf.build_orphan_states(&desired_ids) {
            current_states.entry(id).or_insert(state);
        }
    } else {
        for resource in &sorted_resources {
            current_states.insert(resource.id.clone(), State::not_found(resource.id.clone()));
        }
    }

    // Insert every upstream binding — even when the state file is
    // unreadable — so `resolve_refs_for_plan` knows the binding name
    // and can stamp surviving refs as `(known after upstream apply: <ref>)`
    // instead of leaving the raw dot-form (#2366).
    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for us in &parsed.upstream_states {
        let dir = if us.source.is_absolute() {
            us.source.clone()
        } else {
            base_dir.join(&us.source)
        };
        let state_path = dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);
        let bindings = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|content| check_and_migrate(&content).ok())
            .map(|sf| sf.build_remote_bindings())
            .unwrap_or_default();
        remote_bindings.insert(us.binding.clone(), bindings);
    }

    // Plan-only resolution: stamps surviving upstream refs for display
    // as `(known after upstream apply: <ref>)` (#2366). The fixture
    // loader is already lenient about missing upstream state files (see
    // the loop above that silently skips them).
    let mut resources = sorted_resources.clone();
    resolve_refs_for_plan(&mut resources, &current_states, &remote_bindings)
        .expect("Failed to resolve refs with state");

    // Type-level canonicalization for `Union[String, list(String)]`
    // fields. See #2481, #2511, #2513.
    carina_core::value::canonicalize_resources_with_schemas(&mut resources, wiring.schemas());
    carina_core::value::canonicalize_states_with_schemas(&mut current_states, wiring.schemas());

    normalize_desired_with_ctx(&wiring, &mut resources);
    normalize_state_with_ctx(&wiring, &mut current_states);

    {
        use carina_core::provider::{ProviderNormalizer, ProviderRouter};
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build tokio runtime for merge_default_tags");
        let mut router = ProviderRouter::new();
        for factory in wiring.factories() {
            let attrs = indexmap::IndexMap::new();
            router.add_normalizer(rt.block_on(factory.create_normalizer(&attrs)));
        }
        for provider_config in &parsed.providers {
            if !provider_config.default_tags.is_empty() {
                router.merge_default_tags(
                    &mut resources,
                    &provider_config.default_tags,
                    wiring.schemas(),
                );
            }
        }
    }

    resolve_enum_aliases_with_ctx(&wiring, &mut resources);
    resolve_enum_aliases_in_states(&wiring, &mut current_states);

    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();

    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();

    let mut prev_desired_keys = state_file
        .as_ref()
        .map(|sf| sf.build_desired_keys())
        .unwrap_or_default();

    let orphan_dependencies = if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        sf.build_orphan_dependencies(&desired_ids)
    } else {
        HashMap::new()
    };

    let moved_pairs = crate::wiring::materialize_moved_states(
        &mut current_states,
        &mut prev_desired_keys,
        &mut saved_attrs,
        &parsed.state_blocks,
        &state_file,
    );

    let mut plan = create_plan(
        &resources,
        &current_states,
        &lifecycles,
        wiring.schemas(),
        &saved_attrs,
        &prev_desired_keys,
        &orphan_dependencies,
    );

    cascade_dependent_updates(
        &mut plan,
        &sorted_resources,
        &current_states,
        wiring.schemas(),
    );

    crate::wiring::add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        &state_file,
        &moved_pairs,
        wiring.schemas(),
    );

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();

    FixturePlan {
        plan,
        current_states,
        schemas: wiring.schemas().clone(),
        moved_origins,
        deferred_for_expressions: parsed.deferred_for_expressions,
        export_params: parsed.export_params,
    }
}

/// Collect `Delete`-effect attributes from `current_states` for display.
pub fn delete_attributes_from_plan(
    plan: &Plan,
    current_states: &HashMap<ResourceId, State>,
) -> HashMap<ResourceId, HashMap<String, Value>> {
    plan.effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// Collect the attributes of all live states (for destroy rendering).
pub fn delete_attributes_from_states(
    current_states: &HashMap<ResourceId, State>,
) -> HashMap<ResourceId, HashMap<String, Value>> {
    current_states
        .iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id.clone(), state.attributes.clone()))
        .collect()
}
