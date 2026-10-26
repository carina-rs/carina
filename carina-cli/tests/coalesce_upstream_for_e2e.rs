//! End-to-end regression coverage for carina#3961.
//!
//! A coalesced upstream export must use the export when it exists and the
//! default only when a successfully loaded upstream state omits the export.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::Effect;
use carina_core::resource::{ConcreteValue, Value};

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "carina {operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn write_common_files(dir: &Path) {
    fs::create_dir_all(dir).expect("create project directory");
    fs::write(
        dir.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .expect("write backend configuration");
    fs::write(dir.join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
}

fn write_upstream_main(dir: &Path, export_accounts: bool) {
    let exports = if export_accounts {
        "\nexports { accounts: list(String) = [\"a\", \"b\"] }\n"
    } else {
        "\nexports {}\n"
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let seed = mock.test.resource {{
  name = "seed"
}}
{exports}"#
        ),
    )
    .expect("write upstream main configuration");
}

fn write_downstream(dir: &Path) {
    fs::write(
        dir.join("upstream.crn"),
        "let up = upstream_state { source = \"../up\" }\n",
    )
    .expect("write upstream_state configuration");
    fs::write(
        dir.join("main.crn"),
        r#"for x in (up.accounts ?? []) {
  mock.test.resource {
    name = x
  }
}
"#,
    )
    .expect("write downstream main configuration");
}

fn write_managed_coalesce_main(dir: &Path, use_reference: bool, include_consumer: bool) {
    let parent_name = if use_reference {
        "source.name"
    } else {
        r#""seed""#
    };
    let consumer = if include_consumer {
        r#"
let consumer = mock.test.resource {
  name = producer.comment ?? "d"
}
"#
    } else {
        ""
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let source = mock.test.resource {{
  name = "seed"
}}

let producer = mock.test.resource {{
  name = "producer"
  parent_name = {parent_name}
}}
{consumer}"#,
        ),
    )
    .expect("write managed coalesce configuration");
}

fn plan_json(dir: &Path) -> PlanFile {
    let output = run_cli(dir, &["plan", "--json", "."]);
    assert_success(&output, "plan --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_start = stdout
        .find('{')
        .expect("JSON plan must follow provider and refresh status lines");
    serde_json::from_str(&stdout[json_start..]).unwrap_or_else(|error| {
        panic!(
            "plan output was not a saved-plan JSON document: {error}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn created_names(plan: &PlanFile) -> Vec<String> {
    let mut names = plan
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Create(resource) => match resource.get_attr("name") {
                Some(Value::Concrete(ConcreteValue::String(name))) => {
                    Some(name.expose_plaintext().to_string())
                }
                other => panic!("create must have a concrete name, got {other:?}"),
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn coalesced_upstream_iterable_uses_present_export_then_defaults_when_absent() {
    let root = tempfile::tempdir().expect("create temporary root");
    let upstream = root.path().join("up");
    let downstream = root.path().join("down");
    write_common_files(&upstream);
    write_common_files(&downstream);
    write_upstream_main(&upstream, true);
    write_downstream(&downstream);

    assert_success(&run_cli(&upstream, &["init", "."]), "init upstream");
    assert_success(
        &run_cli(&upstream, &["apply", "--auto-approve", "."]),
        "apply upstream",
    );
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");

    let present = plan_json(&downstream);
    assert_eq!(
        created_names(&present),
        ["a".to_string(), "b".to_string()],
        "a present upstream export must win over the coalesce default",
    );

    write_upstream_main(&upstream, false);
    assert_success(
        &run_cli(&upstream, &["apply", "--auto-approve", "."]),
        "reapply upstream without accounts",
    );

    let absent = plan_json(&downstream);
    assert!(
        absent.plan.effects().is_empty(),
        "a loaded-but-missing export must choose [] with no residual effect: {:#?}",
        absent.plan.effects(),
    );
    assert!(
        absent
            .plan
            .effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::DeferredCreate { .. })),
        "the absent export must not leave a deferred create",
    );
}

#[test]
fn coalesced_upstream_iterable_stays_deferred_when_state_is_unavailable() {
    let root = tempfile::tempdir().expect("create temporary root");
    let upstream = root.path().join("up");
    let downstream = root.path().join("down");
    write_common_files(&upstream);
    write_common_files(&downstream);
    write_upstream_main(&upstream, true);
    write_downstream(&downstream);

    // Deliberately do not initialize or apply the upstream project: its
    // configuration declares `accounts`, but no state snapshot exists yet.
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");

    let unknown = plan_json(&downstream);
    assert!(created_names(&unknown).is_empty());
    assert_eq!(
        unknown
            .plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::DeferredCreate { .. }))
            .count(),
        1,
        "an unavailable upstream snapshot must not turn the loop into zero resources: {:#?}",
        unknown.plan.effects(),
    );
}

#[test]
fn unchanged_ref_authored_managed_producer_defaults_at_plan_and_apply() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    write_managed_coalesce_main(&project, false, false);

    assert_success(&run_cli(&project, &["init", "."]), "init project");
    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply producer fixture",
    );

    // The reference resolves to the already-applied value, so the producer
    // has no planned effect even though its authored input is still deferred
    // before binding resolution. Its refreshed, missing `comment` is therefore
    // definitive absence and must select the fallback.
    write_managed_coalesce_main(&project, true, true);
    let plan = plan_json(&project);
    assert_eq!(created_names(&plan), ["d".to_string()]);
    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply managed coalesce fallback",
    );

    let settled = plan_json(&project);
    assert!(
        settled.plan.effects().is_empty(),
        "the fallback consumer must have been applied without updating its unchanged producer: {:#?}",
        settled.plan.effects(),
    );
}
