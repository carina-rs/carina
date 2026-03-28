//! Plan display snapshot tests.
//!
//! Each test loads a .crn fixture (and optionally a state file), builds a plan
//! using the same logic as `--refresh=false`, formats the plan output, strips
//! ANSI color codes, and asserts the result against an `insta` snapshot.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::resolver::resolve_refs_with_state;
use carina_core::resource::{ResourceId, State};
use carina_core::schema::ResourceSchema;
use carina_state::StateFile;

use crate::DetailLevel;
use crate::commands::validate_and_resolve;
use crate::display::{format_destroy_plan, format_plan};
use crate::wiring::{
    WiringContext, normalize_desired_with_ctx, normalize_state_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
    resolve_enum_aliases_in_states, resolve_enum_aliases_with_ctx,
};

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

/// Build a plan from a .crn fixture and optional state file, mimicking `--refresh=false`.
fn build_plan_from_fixture(
    fixture_dir: &str,
) -> (carina_core::plan::Plan, HashMap<String, ResourceSchema>) {
    let (plan, _, schemas) = build_plan_and_states_from_fixture(fixture_dir);
    (plan, schemas)
}

fn build_plan_and_states_from_fixture(
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    HashMap<ResourceId, State>,
    HashMap<String, ResourceSchema>,
) {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let crn_path = PathBuf::from(format!(
        "{}/tests/fixtures/plan_display/{}/main.crn",
        manifest_dir, fixture_dir
    ));
    let state_path = PathBuf::from(format!(
        "{}/tests/fixtures/plan_display/{}/carina.state.json",
        manifest_dir, fixture_dir
    ));

    // Parse configuration
    let mut parsed = load_configuration(&crn_path).unwrap().parsed;
    let base_dir = get_base_dir(&crn_path);
    validate_and_resolve(&mut parsed, base_dir, false).unwrap();

    // Load state file if present
    let state_file: Option<StateFile> = if state_path.exists() {
        let json = std::fs::read_to_string(&state_path).unwrap();
        Some(serde_json::from_str(&json).unwrap())
    } else {
        None
    };

    // Reconcile identifiers with state (same as plan command)
    let wiring = WiringContext::new();
    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    if let Some(sf) = state_file.as_ref() {
        reconcile_anonymous_identifiers_with_ctx(&wiring, &mut parsed.resources, sf);
    }

    // Sort resources by dependency order
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources).unwrap();

    // Build current states (--refresh=false path)
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    if let Some(sf) = state_file.as_ref() {
        for resource in &sorted_resources {
            let state = sf.build_state_for_resource(resource);
            current_states.insert(resource.id.clone(), state);
        }

        // Include orphaned resources
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

    // Resolve ResourceRef values using state
    let mut resources = sorted_resources.clone();
    resolve_refs_with_state(&mut resources, &current_states);

    // Normalize desired resources (resolve enum identifiers)
    normalize_desired_with_ctx(&wiring, &mut resources);

    // Normalize state enum values to match DSL format
    normalize_state_with_ctx(&wiring, &mut current_states);

    // Merge default_tags from provider configs into resources that support tags
    {
        use carina_core::provider::{ProviderNormalizer, ProviderRouter};
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build tokio runtime for merge_default_tags");
        let mut router = ProviderRouter::new();
        for factory in wiring.factories() {
            let attrs = HashMap::new();
            if let Some(normalizer) = rt.block_on(factory.create_normalizer(&attrs)) {
                router.add_normalizer(normalizer);
            }
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

    // Resolve enum aliases (e.g., "all" -> "-1") in both desired and current states
    resolve_enum_aliases_with_ctx(&wiring, &mut resources);
    resolve_enum_aliases_in_states(&wiring, &mut current_states);

    // Build plan
    let lifecycles = state_file
        .as_ref()
        .map(|sf| sf.build_lifecycles())
        .unwrap_or_default();

    let saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();

    let prev_desired_keys = state_file
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

    // Add state block effects (import/removed/moved)
    crate::wiring::add_state_block_effects(&mut plan, &parsed.state_blocks, &state_file);

    (plan, current_states, wiring.schemas().clone())
}

#[test]
fn snapshot_all_create() {
    let (plan, schemas) = build_plan_from_fixture("all_create");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes() {
    let (plan, _schemas) = build_plan_from_fixture("no_changes");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_mixed_operations() {
    let (plan, schemas) = build_plan_from_fixture("mixed_operations");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_delete_orphan() {
    use carina_core::resource::Value;
    let (plan, current_states, schemas) = build_plan_and_states_from_fixture("delete_orphan");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
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
        .collect();
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_compact() {
    let (plan, _schemas) = build_plan_from_fixture("compact");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let (plan, schemas) = build_plan_from_fixture("map_key_diff");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_enum_display() {
    let (plan, schemas) = build_plan_from_fixture("enum_display");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes_enum() {
    let (plan, _schemas) = build_plan_from_fixture("no_changes_enum");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_full() {
    use carina_core::resource::Value;
    let (plan, current_states, _schemas) = build_plan_and_states_from_fixture("destroy_full");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_orphans() {
    use carina_core::resource::Value;
    let (plan, current_states, _schemas) = build_plan_and_states_from_fixture("destroy_orphans");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_values() {
    let (plan, schemas) = build_plan_from_fixture("default_values");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_read_only_attrs() {
    let (plan, schemas) = build_plan_from_fixture("read_only_attrs");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_explicit() {
    let (plan, schemas) = build_plan_from_fixture("explicit");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Explicit,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_tags() {
    let (plan, schemas) = build_plan_from_fixture("default_tags");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_state_blocks() {
    let (plan, schemas) = build_plan_from_fixture("state_blocks");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_secret_values() {
    use carina_core::resource::Value;
    let (plan, current_states, schemas) = build_plan_and_states_from_fixture("secret_values");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
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
        .collect();
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
    ));
    insta::assert_snapshot!(output);
}

/// Ensure no fixture .crn file has unused `let` bindings.
///
/// `let` should only be used when a binding is referenced by another resource.
/// This test prevents regressions where unnecessary `let` bindings are added
/// to fixture files.
#[test]
fn no_unused_let_bindings_in_fixtures() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixtures_dir = format!("{}/tests/fixtures/plan_display", manifest_dir);

    let mut failures: Vec<(String, Vec<String>)> = Vec::new();

    for entry in std::fs::read_dir(&fixtures_dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let fixture_name = entry.file_name().to_string_lossy().to_string();
        let crn_path = PathBuf::from(format!("{}/{}/main.crn", fixtures_dir, fixture_name));
        if !crn_path.exists() {
            continue;
        }

        let loaded = load_configuration(&crn_path).unwrap();
        let unused = crate::wiring::check_unused_bindings(&loaded.unresolved_parsed);
        if !unused.is_empty() {
            failures.push((fixture_name, unused));
        }
    }

    if !failures.is_empty() {
        let msg: Vec<String> = failures
            .iter()
            .map(|(name, bindings)| {
                format!("  {}: unused let bindings: {}", name, bindings.join(", "))
            })
            .collect();
        panic!(
            "Fixture .crn files must not have unused let bindings:\n{}",
            msg.join("\n")
        );
    }
}
