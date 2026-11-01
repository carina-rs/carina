//! Regression coverage for carina#3003.
//!
//! Wrapping an anonymous resource in a `let` binding must produce a state move
//! even when every create-only identity input is a cross-file resource ref.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus};

use carina_core::resource::{ResourceIdentity, absolute_binding_address_for_test};
use carina_state::{ResourceState, StateFile};

const ANONYMOUS_IDENTITY: &str = "mock_compute_instance_11111111";
const ANONYMOUS_ALLOCATION_IDENTITY: &str = "mock_test_resource_22222222";

fn applied_row(
    resource_type: &str,
    identity: &str,
    binding: Option<&str>,
    identifier: &str,
    attributes: &[(&str, serde_json::Value)],
) -> ResourceState {
    let mut row = ResourceState::try_applied(
        resource_type.to_string(),
        ResourceIdentity::try_from(identity.to_string()).expect("fixture identity is valid"),
        "mock",
        identifier,
    )
    .expect("fixture identifier is non-empty");
    row.binding = binding.map(absolute_binding_address_for_test);
    for (name, value) in attributes {
        row = row.with_attribute(*name, value.clone());
    }
    row
}
fn init_project(project: &Path) {
    let init = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["init", "."])
        .output()
        .expect("run carina init");
    assert!(
        init.status.success(),
        "carina init failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr),
    );
}

fn write_state(project: &Path, state: &StateFile) {
    fs::write(
        project.join("carina.state.json"),
        serde_json::to_vec_pretty(state).expect("serialize state fixture"),
    )
    .expect("write state fixture");
}

fn run_plan(project: &Path) -> (ExitStatus, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(["plan", "--refresh=false", "."])
        .output()
        .expect("run carina plan");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    (output.status, rendered)
}

fn write_dependency_rename_project(project: &Path, explicit_allocation_move: bool) {
    fs::write(project.join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
    let moved = explicit_allocation_move.then(|| {
        format!(
            r#"moved {{
  from = mock.test.resource '{ANONYMOUS_ALLOCATION_IDENTITY}'
  to   = mock.test.resource 'allocation'
}}

"#,
        )
    });
    fs::write(
        project.join("main.crn"),
        format!(
            r#"backend local {{ path = "carina.state.json" }}

{}let nat_gw = mock.compute.Instance {{
  name = allocation.identifier
  cluster_name = subnet.identifier
}}
"#,
            moved.as_deref().unwrap_or_default(),
        ),
    )
    .expect("write named consumer configuration");
    fs::write(
        project.join("network.crn"),
        r#"let allocation = mock.test.resource {
  name = "eip-source"
}

let subnet = mock.test.resource {
  name = "subnet-source"
}
"#,
    )
    .expect("write renamed producer configuration");
}

fn dependency_rename_state() -> StateFile {
    let mut state = StateFile::new();
    state
        .upsert_resource(applied_row(
            "test.resource",
            ANONYMOUS_ALLOCATION_IDENTITY,
            None,
            "eip-123",
            &[
                ("name", serde_json::json!("eip-source")),
                ("identifier", serde_json::json!("eip-123")),
            ],
        ))
        .expect("insert anonymous allocation state");
    state
        .upsert_resource(applied_row(
            "test.resource",
            "subnet",
            Some("subnet"),
            "subnet-123",
            &[
                ("name", serde_json::json!("subnet-source")),
                ("identifier", serde_json::json!("subnet-123")),
            ],
        ))
        .expect("insert subnet state");
    state
        .upsert_resource(applied_row(
            "compute.Instance",
            ANONYMOUS_IDENTITY,
            None,
            "nat-123",
            &[
                ("name", serde_json::json!("eip-123")),
                ("cluster_name", serde_json::json!("subnet-123")),
                ("arn", serde_json::json!("arn:mock:nat-123")),
            ],
        ))
        .expect("insert anonymous consumer state");
    state
}

fn assert_dependency_rename_is_two_moves(status: ExitStatus, rendered: &str) {
    assert!(
        status.success(),
        "dependency rename plan failed\n{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "-> mock.test.resource allocation (moved from: {ANONYMOUS_ALLOCATION_IDENTITY})"
        )),
        "plan must move the anonymous producer to its new binding\n{rendered}",
    );
    assert!(
        rendered.contains(&format!(
            "-> mock.compute.Instance nat_gw (moved from: {ANONYMOUS_IDENTITY})"
        )),
        "plan must resolve the consumer through the producer's projected binding\n{rendered}",
    );
    assert!(
        rendered.contains("Plan: 0 to add, 0 to change, 0 to destroy, 2 to move."),
        "plan must contain exactly the two state moves\n{rendered}",
    );
}

#[test]
fn multifile_plan_moves_dependency_and_consumer_renamed_in_same_edit() {
    let project = tempfile::tempdir().expect("create temporary project");
    write_dependency_rename_project(project.path(), false);
    init_project(project.path());
    write_state(project.path(), &dependency_rename_state());

    let (status, rendered) = run_plan(project.path());

    assert_dependency_rename_is_two_moves(status, &rendered);
}

#[test]
fn multifile_plan_projects_explicitly_moved_anonymous_dependency_for_consumer_rename() {
    let project = tempfile::tempdir().expect("create temporary project");
    write_dependency_rename_project(project.path(), true);
    init_project(project.path());
    write_state(project.path(), &dependency_rename_state());

    let (status, rendered) = run_plan(project.path());

    assert_dependency_rename_is_two_moves(status, &rendered);
}

#[test]
fn multifile_plan_moves_anonymous_resource_whose_create_only_attrs_are_refs() {
    let project = tempfile::tempdir().expect("create temporary project");
    fs::write(project.path().join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
    fs::write(
        project.path().join("main.crn"),
        r#"backend local { path = "carina.state.json" }

let nat_gw = mock.compute.Instance {
  name = allocation.identifier
  cluster_name = subnet.identifier
}
"#,
    )
    .expect("write named resource configuration");
    fs::write(
        project.path().join("network.crn"),
        r#"let allocation = mock.test.resource {
  name = "eip-source"
}

let subnet = mock.test.resource {
  name = "subnet-source"
}
"#,
    )
    .expect("write referenced sibling bindings");

    init_project(project.path());

    let mut state = StateFile::new();
    state
        .upsert_resource(applied_row(
            "test.resource",
            "allocation",
            Some("allocation"),
            "eip-123",
            &[
                ("name", serde_json::json!("eip-source")),
                ("identifier", serde_json::json!("eip-123")),
            ],
        ))
        .expect("insert allocation state");
    state
        .upsert_resource(applied_row(
            "test.resource",
            "subnet",
            Some("subnet"),
            "subnet-123",
            &[
                ("name", serde_json::json!("subnet-source")),
                ("identifier", serde_json::json!("subnet-123")),
            ],
        ))
        .expect("insert subnet state");
    state
        .upsert_resource(applied_row(
            "compute.Instance",
            ANONYMOUS_IDENTITY,
            None,
            "nat-123",
            &[
                ("name", serde_json::json!("eip-123")),
                ("cluster_name", serde_json::json!("subnet-123")),
                ("arn", serde_json::json!("arn:mock:nat-123")),
            ],
        ))
        .expect("insert anonymous target state");
    write_state(project.path(), &state);

    let (status, rendered) = run_plan(project.path());

    assert!(
        status.success(),
        "multifile reference rename plan failed\n{rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "-> mock.compute.Instance nat_gw (moved from: {ANONYMOUS_IDENTITY})"
        )),
        "plan must show the anonymous row moving to the new binding\n{rendered}"
    );
    assert!(
        rendered.contains("Plan: 0 to add, 0 to change, 0 to destroy, 1 to move."),
        "plan must be a pure move, not a delete/create replacement\n{rendered}"
    );
    assert!(
        !rendered.contains("+ mock.compute.Instance nat_gw")
            && !rendered.contains(&format!("- mock.compute.Instance {ANONYMOUS_IDENTITY}")),
        "plan must not contain either half of a delete/create pair\n{rendered}"
    );
}

#[test]
fn multifile_plan_does_not_match_only_the_resolvable_create_only_subset() {
    let project = tempfile::tempdir().expect("create temporary project");
    fs::write(project.path().join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
    fs::write(
        project.path().join("main.crn"),
        r#"backend local { path = "carina.state.json" }

let nat_gw = mock.compute.Instance {
  name = allocation.identifier
  cluster_name = new_subnet.identifier
}
"#,
    )
    .expect("write named resource configuration");
    fs::write(
        project.path().join("network.crn"),
        r#"let allocation = mock.test.resource {
  name = "eip-source"
}

let new_subnet = mock.test.resource {
  name = "new-subnet-source"
}
"#,
    )
    .expect("write referenced sibling bindings");
    init_project(project.path());

    let mut state = StateFile::new();
    state
        .upsert_resource(applied_row(
            "test.resource",
            "allocation",
            Some("allocation"),
            "eip-123",
            &[
                ("name", serde_json::json!("eip-source")),
                ("identifier", serde_json::json!("eip-123")),
            ],
        ))
        .expect("insert allocation state");
    state
        .upsert_resource(applied_row(
            "compute.Instance",
            ANONYMOUS_IDENTITY,
            None,
            "nat-123",
            &[
                ("name", serde_json::json!("eip-123")),
                ("cluster_name", serde_json::json!("old-subnet-123")),
                ("arn", serde_json::json!("arn:mock:nat-123")),
            ],
        ))
        .expect("insert anonymous target state");
    write_state(project.path(), &state);

    let (status, rendered) = run_plan(project.path());
    assert!(status.success(), "plan failed\n{rendered}");
    assert!(
        !rendered.contains(&format!("moved from: {ANONYMOUS_IDENTITY}")),
        "an unresolved create-only ref must prevent subset-based rename matching\n{rendered}"
    );
}
