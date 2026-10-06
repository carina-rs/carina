//! End-to-end regression coverage for carina#3826 and carina#3829.
//!
//! Anonymous resources must never collapse while their identities are still
//! pending. Schema-stable and let-bound resources both reach the real plan;
//! multiple mutable-attribute-derived resources are rejected before planning.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::Effect;
use carina_core::hint::ProjectCommand;
use carina_state::StateFile;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        copy_fixture(&project);
        Self {
            _temp: temp,
            project,
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn write_main(&self, resources: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                "backend local {{ path = \"carina.state.json\" }}\n\nprovider mock {{}}\n\n{resources}"
            ),
        )
        .expect("write project configuration");
    }

    fn write_module(&self, relative_path: &str, source: &str) {
        let module = self.project.join(relative_path);
        fs::create_dir_all(&module).expect("create module directory");
        fs::write(module.join("main.crn"), source).expect("write module configuration");
    }

    fn state(&self) -> StateFile {
        carina_state::check_and_migrate(
            &fs::read_to_string(self.project.join("carina.state.json")).expect("read local state"),
        )
        .expect("load local state")
        .into_state()
    }
}

const ALPHA: &str = r#"mock.test.resource {
  name = "alpha"
}
"#;

const ALPHA_AND_BETA: &str = r#"mock.test.resource {
  name = "alpha"
}

mock.test.resource {
  name = "beta"
}
"#;

const ATTRIBUTE_DERIVED_ALPHA_AND_BETA: &str = r#"mock.test.Thing {
  name = "alpha"
}

mock.test.Thing {
  name = "beta"
}
"#;

const LET_ALPHA_AND_BETA: &str = r#"let alpha = mock.test.resource {
  name = "alpha"
}

let beta = mock.test.resource {
  name = "beta"
}
"#;

const ATTRIBUTE_DERIVED_IDENTITY_ERROR: &str = "Anonymous resource identity is derived from \
mutable attributes for multiple 'mock.test.Thing' declarations using provider instance \
'<default>' in the root scope. Use `let` bindings to give them distinct stable identities.";

const ONE_RESOURCE_MODULE: &str = r#"arguments {
  n: String
}

mock.test.Thing {
  name = n
}
"#;

const TWO_RESOURCE_MODULE: &str = r#"arguments {
  n: String
}

mock.test.Thing {
  name = "${n}-1"
}

mock.test.Thing {
  name = "${n}-2"
}
"#;

const DISTINCT_NESTED_MODULE: &str = r#"arguments {
  n: String
}

let inner = use { source = "../inner" }

let i1 = inner { n = "${n}-1" }
let i2 = inner { n = "${n}-2" }
"#;

const IDENTICAL_NESTED_MODULE: &str = r#"arguments {
  n: String
}

let inner = use { source = "../inner" }

let i1 = inner { n = n }
let i2 = inner { n = n }
"#;

fn copy_fixture(destination: &Path) {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anonymous_resource_plan");
    for entry in fs::read_dir(fixture).expect("read anonymous-resource fixture") {
        let entry = entry.expect("read fixture entry");
        fs::copy(entry.path(), destination.join(entry.file_name())).expect("copy fixture file");
    }
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn unused_binding_warning(binding: &str) -> String {
    format!("Unused let binding '{binding}'. Consider using an anonymous resource instead.")
}

#[test]
fn unknown_anonymous_mock_type_is_rejected_before_plan_and_creates_nothing() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let bootstrap = mock.test.resource {
  name = "bootstrap"
}
"#,
    );
    assert_success("carina init", &scenario.carina(&["init", "."]));

    scenario.write_main(
        r#"mock.unknown.Widget {
  name = "must-not-be-created"
}
"#,
    );

    for command in ["validate", "plan"] {
        let output = scenario.carina(&[command, "."]);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success()
                && stderr.contains("Unknown resource type: mock.unknown.Widget"),
            "carina {command} must reject an unknown built-in mock resource type\n\
             status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status,
        );
    }

    assert!(
        !scenario.project.join("carina.state.json").exists()
            && !scenario.project.join(".carina/state.json").exists(),
        "validation and planning must not create either backend or mock-provider state",
    );
}

#[test]
fn providerless_mock_resource_uses_builtin_provider_and_catalog_consistently() {
    let scenario = Scenario::new();
    fs::write(
        scenario.project.join("main.crn"),
        r#"backend local { path = "carina.state.json" }

mock.test.resource {
  name = "a"
}
"#,
    )
    .expect("write provider-less mock configuration");

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success("carina validate", &scenario.carina(&["validate", "."]));

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("carina plan", &plan);
    let stdout = String::from_utf8_lossy(&plan.stdout);
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert!(
        stdout.contains("Using mock provider") && stdout.contains("Execution Plan:"),
        "provider-less mock resources must be served and planned by the built-in provider\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        !stderr.contains("has no schema"),
        "routing and schema activation must not disagree\nstderr:\n{stderr}",
    );
}

#[test]
fn validate_rejects_anonymous_resources_whose_provider_schema_is_unavailable() {
    let cases = [
        (
            "undeclared provider",
            "# provider intentionally not declared\n",
            r#"aws.s3.Bucket {
  bucket = "x"
}
"#,
            "anonymous aws.s3.Bucket resource needs provider 'aws', which is not declared or could not be loaded; declare provider 'aws' so its schema is available",
        ),
        (
            "different declared provider",
            "provider mock {}\n",
            r#"aws.s3.Bucket {
  bucket = "x"
}
"#,
            "anonymous aws.s3.Bucket resource needs provider 'aws', which is not declared or could not be loaded; declare provider 'aws' so its schema is available",
        ),
        (
            "named provider without kind default",
            "let secondary = provider mock {}\n",
            r#"mock.test.resource {
  name = "x"

  directives {
    provider = secondary
  }
}
"#,
            "anonymous mock.test.resource resource needs provider instance 'secondary' of 'mock', which is not declared or could not be loaded; declare provider 'mock' so its schema is available",
        ),
    ];

    for (label, provider_source, resource_source, expected) in cases {
        let scenario = Scenario::new();
        fs::write(scenario.project.join("providers.crn"), provider_source)
            .expect("write sibling provider configuration");
        fs::write(scenario.project.join("main.crn"), resource_source)
            .expect("write repro resource configuration");

        let validate = scenario.carina(&["validate", "."]);
        let stdout = String::from_utf8_lossy(&validate.stdout);
        let stderr = String::from_utf8_lossy(&validate.stderr);
        assert!(
            !validate.status.success() && stderr.contains(expected),
            "{label}: validate must reject the schema-less anonymous resource\n\
             expected: {expected}\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            validate.status,
        );
    }
}

#[test]
fn validate_rejects_multiple_attribute_derived_anonymous_resources() {
    let scenario = Scenario::new();
    scenario.write_main(ATTRIBUTE_DERIVED_ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let validate = scenario.carina(&["validate", "."]);
    let stderr = String::from_utf8_lossy(&validate.stderr);
    assert!(
        !validate.status.success() && stderr.contains(ATTRIBUTE_DERIVED_IDENTITY_ERROR),
        "validate must reject ambiguous attribute-derived anonymous identities\n\
         status: {}\nstdout:\n{}\nstderr:\n{stderr}",
        validate.status,
        String::from_utf8_lossy(&validate.stdout),
    );
}

#[test]
fn validate_does_not_warn_for_attribute_derived_bindings_required_across_sibling_files() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let alpha = mock.test.Thing {
  name = "alpha"
}
"#,
    );
    fs::write(
        scenario.project.join("beta.crn"),
        r#"let beta = mock.test.Thing {
  name = "beta"
}
"#,
    )
    .expect("write sibling resource");

    assert_success("carina init", &scenario.carina(&["init", "."]));
    let validate = scenario.carina(&["validate", "."]);
    assert_success("carina validate", &validate);
    let stdout = String::from_utf8_lossy(&validate.stdout);

    assert!(
        !stdout.contains(&unused_binding_warning("alpha"))
            && !stdout.contains(&unused_binding_warning("beta")),
        "identity-required bindings must not receive contradictory unused warnings\n{stdout}",
    );
}

#[test]
fn validate_still_warns_for_single_unused_attribute_derived_binding() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let alpha = mock.test.Thing {
  name = "alpha"
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    let validate = scenario.carina(&["validate", "."]);
    assert_success("carina validate", &validate);
    let stdout = String::from_utf8_lossy(&validate.stdout);

    assert!(
        stdout.contains(&unused_binding_warning("alpha")),
        "a lone attribute-derived binding is safe to anonymize and must still warn\n{stdout}",
    );
}

#[test]
fn validate_still_warns_for_unused_stable_binding() {
    let scenario = Scenario::new();
    scenario.write_main(
        r#"let alpha = mock.test.resource {
  name = "alpha"
}
"#,
    );

    assert_success("carina init", &scenario.carina(&["init", "."]));
    let validate = scenario.carina(&["validate", "."]);
    assert_success("carina validate", &validate);
    let stdout = String::from_utf8_lossy(&validate.stdout);

    assert!(
        stdout.contains(&unused_binding_warning("alpha")),
        "a stable binding does not prevent an identity conflict and must still warn\n{stdout}",
    );
}

#[test]
fn plan_rejects_multiple_attribute_derived_anonymous_resources() {
    let scenario = Scenario::new();
    scenario.write_main(ATTRIBUTE_DERIVED_ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let stderr = String::from_utf8_lossy(&plan.stderr);
    assert!(
        !plan.status.success() && stderr.contains(ATTRIBUTE_DERIVED_IDENTITY_ERROR),
        "plan must reject ambiguous attribute-derived anonymous identities\n\
         status: {}\nstdout:\n{}\nstderr:\n{stderr}",
        plan.status,
        String::from_utf8_lossy(&plan.stdout),
    );
}

#[test]
fn stable_anonymous_resources_both_reach_the_plan() {
    let scenario = Scenario::new();

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    assert_success("carina plan", &plan);

    let stdout = String::from_utf8(plan.stdout).expect("plan stdout is UTF-8");
    assert!(
        stdout.contains("Plan: 2 to add, 0 to change, 0 to destroy."),
        "both anonymous resources must appear in the plan summary.\nstdout:\n{stdout}"
    );

    let saved_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("plan.json")).expect("read saved plan"),
    )
    .expect("deserialize saved plan");
    let creates = saved_plan
        .plan
        .effects()
        .iter()
        .filter(|effect| matches!(effect, Effect::Create(_)))
        .count();
    assert_eq!(creates, 2, "both anonymous creates must be serialized");
}

#[test]
fn apply_two_anonymous_resources_then_plan_is_clean_and_destroy_removes_both() {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let apply = scenario.carina(&["apply", "--auto-approve", "."]);
    assert_success("initial carina apply", &apply);

    let plan = scenario.carina(&["plan", "."]);
    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let plan_stderr = String::from_utf8_lossy(&plan.stderr);

    let destroy = scenario.carina(&["destroy", "--auto-approve", "."]);
    let state = scenario.state();

    assert!(
        plan.status.success()
            && plan_stdout.contains("No changes")
            && destroy.status.success()
            && state.resources().is_empty(),
        "apply must converge and destroy must remove both rows\n\
         plan stdout:\n{plan_stdout}\nplan stderr:\n{plan_stderr}\n\
         destroy stdout:\n{}\ndestroy stderr:\n{}\nremaining rows: {}",
        String::from_utf8_lossy(&destroy.stdout),
        String::from_utf8_lossy(&destroy.stderr),
        state.resources().len(),
    );
}

fn assert_kept_anonymous_resource_state_block_collision(
    state_block: impl FnOnce(&str) -> String,
    expected_collision: impl FnOnce(&str) -> String,
) {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA);

    assert_success("carina init", &scenario.carina(&["init", "."]));
    assert_success(
        "apply alpha",
        &scenario.carina(&["apply", "--auto-approve", "."]),
    );

    let initial_state = scenario.state();
    let initial_entry = initial_state
        .resources()
        .iter()
        .find(|entry| entry.provider == "mock" && entry.resource_type == "test.resource")
        .expect("alpha state entry");
    let initial_identity = initial_entry.identity.as_str().to_string();
    let initial_state_bytes =
        fs::read(scenario.project.join("carina.state.json")).expect("read initial state bytes");
    let collision = expected_collision(&initial_identity);

    scenario.write_main(&format!("{ALPHA}\n{}", state_block(&initial_identity)));

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let plan_output = format!(
        "{}{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        !plan.status.success() && plan_output.contains(&collision),
        "plan must reject a state-block source that is also a late-resolved desired identity\n\
         expected collision: {collision}\nstatus: {}\noutput:\n{plan_output}",
        plan.status,
    );

    let apply = scenario.carina(&["apply", "--auto-approve", "."]);
    let apply_output = format!(
        "{}{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );
    let final_state_bytes =
        fs::read(scenario.project.join("carina.state.json")).expect("read final state bytes");
    let final_state = scenario.state();
    assert!(
        !apply.status.success()
            && apply_output.contains(&collision)
            && !apply_output.lines().any(|line| line.contains("Create"))
            && final_state_bytes == initial_state_bytes
            && final_state.resources().len() == 1
            && final_state.resources()[0].identity.as_str() == initial_identity,
        "apply must reject the collision before provider mutation or state write\n\
         expected collision: {collision}\nstatus: {}\noutput:\n{apply_output}\n\
         initial state:\n{}\nfinal state:\n{}",
        apply.status,
        String::from_utf8_lossy(&initial_state_bytes),
        String::from_utf8_lossy(&final_state_bytes),
    );
}

#[test]
fn moved_from_kept_anonymous_resource_fails_before_create() {
    assert_kept_anonymous_resource_state_block_collision(
        |identity| {
            format!(
                "moved {{\n  from = mock.test.resource '{identity}'\n  to = mock.test.resource 'other'\n}}\n"
            )
        },
        |identity| {
            format!(
                "moved/rename pair from mock.test.resource {identity} collides with a desired resource"
            )
        },
    );
}

#[test]
fn removed_from_kept_anonymous_resource_fails_before_create() {
    assert_kept_anonymous_resource_state_block_collision(
        |identity| format!("removed {{\n  from = mock.test.resource '{identity}'\n}}\n"),
        |identity| {
            format!(
                "removed block from mock.test.resource {identity} collides with desired resource mock.test.resource {identity}"
            )
        },
    );
}

#[test]
fn saved_plan_apply_for_two_let_bound_resources_converges() {
    let scenario = Scenario::new();
    scenario.write_main(LET_ALPHA_AND_BETA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let plan = scenario.carina(&["plan", "--out", "plan.json", "."]);
    assert_success("carina plan --out", &plan);
    let apply = scenario.carina(&["apply", "--auto-approve", "plan.json"]);
    let replan = scenario.carina(&["plan", "."]);

    let plan_stdout = String::from_utf8_lossy(&plan.stdout);
    let replan_stdout = String::from_utf8_lossy(&replan.stdout);
    let state_rows = scenario.state().resources().len();
    assert!(
        plan_stdout.contains("Plan: 2 to add, 0 to change, 0 to destroy.")
            && apply.status.success()
            && replan.status.success()
            && replan_stdout.contains("No changes")
            && state_rows == 2,
        "saved-plan apply must preserve both let-bound resources and converge\n\
         plan stdout:\n{plan_stdout}\nplan stderr:\n{}\n\
         apply status: {}\napply stdout:\n{}\napply stderr:\n{}\n\
         replan stdout:\n{replan_stdout}\nreplan stderr:\n{}\nstate rows: {state_rows}",
        String::from_utf8_lossy(&plan.stderr),
        apply.status,
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr),
        String::from_utf8_lossy(&replan.stderr),
    );
}

#[test]
fn nested_bound_module_instances_have_distinct_anonymous_identity_scopes() {
    let scenario = Scenario::new();
    scenario.write_module("inner", ONE_RESOURCE_MODULE);
    scenario.write_module("outer", DISTINCT_NESTED_MODULE);
    scenario.write_main(
        r#"let outer = use { source = "./outer" }

let x = outer { n = "a" }
"#,
    );

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let plan = scenario.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
    let stdout = String::from_utf8_lossy(&plan.stdout);
    let saved_plan: PlanFile = serde_json::from_str(
        &fs::read_to_string(scenario.project.join("plan.json")).unwrap_or_default(),
    )
    .unwrap_or_else(|error| panic!("read nested-module plan: {error}\nstdout:\n{stdout}"));
    let identities = saved_plan
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Create(resource) => resource.id.identity_str().map(str::to_string),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();

    assert!(
        plan.status.success()
            && stdout.contains("Plan: 2 to add, 0 to change, 0 to destroy.")
            && identities.len() == 2
            && identities
                .iter()
                .all(|identity| identity.starts_with("x.mock_test_thing_")),
        "x.i1 and x.i2 must be distinct conflict scopes\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{}",
        plan.status,
        String::from_utf8_lossy(&plan.stderr),
    );
}

#[test]
fn identical_nested_module_arguments_fail_cleanly_without_dropping_a_resource() {
    let scenario = Scenario::new();
    scenario.write_module("inner", ONE_RESOURCE_MODULE);
    scenario.write_module("outer", IDENTICAL_NESTED_MODULE);
    scenario.write_main(
        r#"let outer = use { source = "./outer" }

let x = outer { n = "a" }
"#,
    );

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let stderr = String::from_utf8_lossy(&plan.stderr);

    assert!(
        !plan.status.success()
            && stderr.contains("Anonymous resource identifier collision")
            && !String::from_utf8_lossy(&plan.stdout).contains("Plan:")
            && !stderr.contains("panicked"),
        "identical nested identities must return a typed plan error without dropping a resource\n\
         status: {}\nstdout:\n{}\nstderr:\n{stderr}",
        plan.status,
        String::from_utf8_lossy(&plan.stdout),
    );
}

#[test]
fn module_conflict_errors_use_authored_scope_names() {
    let bound = Scenario::new();
    bound.write_module("module", TWO_RESOURCE_MODULE);
    bound.write_main(
        r#"let m = use { source = "./module" }

let x = m { n = "a" }
"#,
    );
    assert_success("bound carina init", &bound.carina(&["init", "."]));
    let bound_plan = bound.carina(&["plan", "--refresh=false", "."]);
    let bound_stderr = String::from_utf8_lossy(&bound_plan.stderr);
    let bound_error = "Anonymous resource identity is derived from mutable attributes for \
multiple 'mock.test.Thing' declarations using provider instance '<default>' in module instance \
'x'. Use `let` bindings to give them distinct stable identities.";

    let anonymous = Scenario::new();
    anonymous.write_module("module", TWO_RESOURCE_MODULE);
    anonymous.write_main(
        r#"let m = use { source = "./module" }

m { n = "a" }
"#,
    );
    assert_success("anonymous carina init", &anonymous.carina(&["init", "."]));
    let anonymous_plan = anonymous.carina(&["plan", "--refresh=false", "."]);
    let anonymous_stderr = String::from_utf8_lossy(&anonymous_plan.stderr);
    let anonymous_error = "Anonymous resource identity is derived from mutable attributes for \
multiple 'mock.test.Thing' declarations using provider instance '<default>' in an anonymous \
call of module 'm'. Use `let` bindings to give them distinct stable identities.";

    assert!(
        !bound_plan.status.success() && bound_stderr.contains(bound_error),
        "bound module scope must use the authored binding path\nstdout:\n{}\nstderr:\n{bound_stderr}",
        String::from_utf8_lossy(&bound_plan.stdout),
    );
    assert!(
        !anonymous_plan.status.success()
            && anonymous_stderr.contains(anonymous_error)
            && !anonymous_stderr.contains("m_"),
        "anonymous module scope must use the authored module name\nstdout:\n{}\nstderr:\n{anonymous_stderr}",
        String::from_utf8_lossy(&anonymous_plan.stdout),
    );
}

#[test]
fn plan_reports_how_to_repair_a_legacy_empty_identity_row() {
    let scenario = Scenario::new();
    scenario.write_main(ALPHA);

    let init = scenario.carina(&["init", "."]);
    assert_success("carina init", &init);

    let invalid_state = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "serial": 1,
        "lineage": "empty-identity-error-e2e",
        "carina_version": "older-carina",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "",
                "provider": "mock",
                "identifier": "legacy-resource-123",
                "attributes": {}
            }
        ],
        "exports": {}
    });
    fs::write(
        scenario.project.join("carina.state.json"),
        serde_json::to_vec_pretty(&invalid_state).expect("serialize invalid state fixture"),
    )
    .expect("write invalid state fixture");

    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert!(
        !plan.status.success(),
        "carina plan must reject the empty identity row\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr),
    );

    let stderr = String::from_utf8(plan.stderr).expect("plan stderr is UTF-8");
    let project_path = scenario
        .project
        .canonicalize()
        .expect("canonicalize temporary project");
    let state_path = project_path.join("carina.state.json");
    let plan_command = ProjectCommand::new("plan", &project_path);
    assert!(
        stderr.contains("resources[0]")
            && stderr.contains("provider=\"mock\"")
            && stderr.contains("resource_type=\"test.resource\"")
            && stderr.contains("identifier=\"legacy-resource-123\"")
            && stderr.contains(&state_path.display().to_string()),
        "error must identify the exact state row:\n{stderr}"
    );
    assert!(
        !stderr.contains("Failed to parse state file")
            && stderr.contains(&format!(
                "Back up the state file, then remove this row. Run `{plan_command}`; the resource \
                 that owned it appears as a create with its newly assigned identity. Put the row \
                 back with `identity` set to that value (keep its `identifier` and attributes), \
                 or leave it removed if the resource is no longer managed."
            )),
        "error must preserve the complete repair instruction:\n{stderr}"
    );
}
