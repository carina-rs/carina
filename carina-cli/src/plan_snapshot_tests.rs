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
use carina_state::StateFile;

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
fn build_plan_from_fixture(fixture_dir: &str) -> carina_core::plan::Plan {
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

    plan
}

#[test]
fn snapshot_all_create() {
    let plan = build_plan_from_fixture("all_create");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes() {
    let plan = build_plan_from_fixture("no_changes");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_mixed_operations() {
    let plan = build_plan_from_fixture("mixed_operations");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_delete_orphan() {
    let plan = build_plan_from_fixture("delete_orphan");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_compact() {
    let plan = build_plan_from_fixture("compact");
    let output = strip_ansi(&format_plan(&plan, true));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let plan = build_plan_from_fixture("map_key_diff");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_enum_display() {
    let plan = build_plan_from_fixture("enum_display");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes_enum() {
    let plan = build_plan_from_fixture("no_changes_enum");
    let output = strip_ansi(&format_plan(&plan, false));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_full() {
    let plan = build_plan_from_fixture("destroy_full");
    let output = strip_ansi(&format_destroy_plan(&plan));
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
