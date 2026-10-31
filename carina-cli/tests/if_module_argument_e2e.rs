//! End-to-end coverage for module-argument `if` conditions (carina#4149).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fixture_source(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/if_module_argument")
        .join(name)
}

struct Fixture {
    _temp: tempfile::TempDir,
    choice: PathBuf,
    caller: PathBuf,
}

impl Fixture {
    fn copy() -> Self {
        let temp = tempfile::tempdir().expect("create module-argument if fixture");
        let choice = temp.path().join("choice");
        let caller = temp.path().join("caller");
        std::fs::create_dir_all(&choice).expect("create copied choice module");
        std::fs::create_dir_all(&caller).expect("create copied caller project");
        std::fs::copy(
            fixture_source("choice").join("main.crn"),
            choice.join("main.crn"),
        )
        .expect("copy choice module");
        for file in ["backend.crn", "main.crn"] {
            std::fs::copy(fixture_source("caller").join(file), caller.join(file))
                .unwrap_or_else(|error| panic!("copy caller {file}: {error}"));
        }
        Self {
            _temp: temp,
            choice,
            caller,
        }
    }
}

fn run(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_dropped_conditional_binding(label: &str, output: &Output) {
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded:\n{}",
        combined_output(output),
    );
    let output = combined_output(output);
    assert!(
        output.contains("binding 'a' is defined only in a dropped branch")
            && output.contains("condition `flag`")
            && output.contains("choice/main.crn")
            && !output.contains("(unresolved module argument: flag)")
            && !output.contains("unknown binding 'a'"),
        "{label} did not use the dedicated resolver diagnostic:\n{output}",
    );
}

struct ConditionalReferenceFixture {
    _temp: tempfile::TempDir,
    selected: PathBuf,
    dropped: PathBuf,
}

struct ConditionalDependencyFixture {
    _temp: tempfile::TempDir,
    selected_argument: PathBuf,
    selected_reference: PathBuf,
}

struct GuardedReferenceFixture {
    _temp: tempfile::TempDir,
    caller: PathBuf,
}

impl GuardedReferenceFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create guarded-reference fixture");
        let choice = temp.path().join("choice");
        let caller = temp.path().join("caller");
        for directory in [&choice, &caller] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            choice.join("main.crn"),
            r#"arguments { flag: Bool }
let a = if flag {
  mock.test.resource { name = "a" }
}
let b = mock.test.resource {
  name = "b"
  comment = if flag { a.name } else { "none" }
}
attributes {
  n: String = if flag { a.name } else { "none" }
}
"#,
        )
        .unwrap();
        std::fs::write(
            caller.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
        )
        .unwrap();
        std::fs::write(
            caller.join("main.crn"),
            r#"let choice = use { source = "../choice" }
let off = choice { flag = false }
let on = choice { flag = true }
"#,
        )
        .unwrap();
        Self {
            _temp: temp,
            caller,
        }
    }
}

impl ConditionalDependencyFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create conditional-dependency fixture");
        let choice = temp.path().join("choice");
        let selected_argument = temp.path().join("selected-argument");
        let selected_reference = temp.path().join("selected-reference");
        for directory in [&choice, &selected_argument, &selected_reference] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            choice.join("main.crn"),
            r#"arguments {
  flag: Bool
  n: String = "d"
}
let a = mock.test.resource { name = "a" }
let consumer = mock.test.resource {
  name = if flag { n } else { a.name }
}
"#,
        )
        .unwrap();
        for (directory, flag) in [(&selected_argument, true), (&selected_reference, false)] {
            std::fs::write(
                directory.join("backend.crn"),
                "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
            )
            .unwrap();
            std::fs::write(
                directory.join("main.crn"),
                format!(
                    "let choice = use {{ source = \"../choice\" }}\nlet on = choice {{ flag = {flag} }}\n"
                ),
            )
            .unwrap();
        }
        Self {
            _temp: temp,
            selected_argument,
            selected_reference,
        }
    }
}

fn plan_json(project: &Path) -> serde_json::Value {
    assert_success("conditional dependency init", &run(project, &["init", "."]));
    let plan = run(project, &["plan", "--json", "--refresh=false", "."]);
    assert_success("conditional dependency JSON plan", &plan);
    serde_json::from_slice(&plan.stdout).unwrap_or_else(|error| {
        panic!(
            "plan stdout was not JSON: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&plan.stdout),
            String::from_utf8_lossy(&plan.stderr),
        )
    })
}

fn resource_dependency_bindings<'a>(
    resources: &'a serde_json::Value,
    binding: &str,
) -> &'a serde_json::Value {
    resources
        .as_array()
        .expect("resource collection")
        .iter()
        .find(|resource| {
            resource.get("binding").and_then(serde_json::Value::as_str) == Some(binding)
        })
        .and_then(|resource| resource.get("dependency_bindings"))
        .unwrap_or_else(|| panic!("missing dependency metadata for {binding}: {resources:#}"))
}

impl ConditionalReferenceFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create conditional-reference fixture");
        let leaf = temp.path().join("leaf");
        let choice = temp.path().join("choice");
        let selected = temp.path().join("selected");
        let dropped = temp.path().join("dropped");
        for directory in [&leaf, &choice, &selected, &dropped] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            leaf.join("main.crn"),
            r#"attributes {
  value: String = "leaf"
}
"#,
        )
        .unwrap();
        std::fs::write(
            choice.join("main.crn"),
            r#"arguments { flag: Bool }
let leaf = use { source = "../leaf" }

let a = if flag {
  mock.test.resource { name = "a" }
}

let x = if flag {
  leaf {}
}

let inside = if flag {
  mock.test.resource {
    name = "inside"
    parent_name = a.name
  }
}

let b = mock.test.resource {
  name = "b"
  parent_name = a.name
  comment = x.value
}
"#,
        )
        .unwrap();
        for (directory, flag) in [(&selected, true), (&dropped, false)] {
            std::fs::write(
                directory.join("backend.crn"),
                "backend local { path = \"carina.state.json\" }\nprovider mock {}\n",
            )
            .unwrap();
            std::fs::write(
                directory.join("main.crn"),
                format!(
                    "let choice = use {{ source = \"../choice\" }}\nlet instance = choice {{ flag = {flag} }}\n"
                ),
            )
            .unwrap();
        }
        Self {
            _temp: temp,
            selected,
            dropped,
        }
    }
}

#[test]
fn standalone_module_and_caller_validate() {
    let fixture = Fixture::copy();
    assert_success(
        "standalone choice module validation",
        &run(&fixture.choice, &["validate", "."]),
    );

    assert_success(
        "caller validation",
        &run(&fixture.caller, &["validate", "."]),
    );
}

#[test]
fn standalone_module_plan_rejects_unexpanded_template() {
    let fixture = Fixture::copy();
    assert_success(
        "standalone choice module init",
        &run(&fixture.choice, &["init", "."]),
    );
    let plan = run(&fixture.choice, &["plan", "--refresh=false", "."]);
    assert!(
        !plan.status.success(),
        "planning a standalone module template unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr),
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr),
    );
    assert!(
        output.contains("cannot publish a reusable module template")
            && output.contains("argument-dependent `if`")
            && !output.contains("Internal parser error"),
        "standalone plan did not explain the unresolved template:\n{output}",
    );
}

#[test]
fn real_plan_resolves_true_and_false_module_instances() {
    let fixture = Fixture::copy();
    assert_success("caller init", &run(&fixture.caller, &["init", "."]));

    let plan = run(&fixture.caller, &["plan", "--refresh=false", "."]);
    assert_success("caller plan", &plan);
    let stdout = String::from_utf8(plan.stdout).expect("plan stdout must be UTF-8");
    assert!(
        stdout.contains("+ enabled: String = 'yes'")
            && stdout.contains("+ disabled: String = 'no'"),
        "plan did not materialize both conditional module attributes:\n{stdout}",
    );
    assert!(
        stdout.contains("enabled.conditional"),
        "true structural branch was not planned:\n{stdout}",
    );
    assert!(
        !stdout.contains("disabled.conditional"),
        "false structural branch unexpectedly produced a resource:\n{stdout}",
    );
}

#[test]
fn conditional_binding_references_validate_after_selection_and_report_dropped_branches() {
    let fixture = ConditionalReferenceFixture::new();

    let selected_validate = run(&fixture.selected, &["validate", "."]);
    assert_success(
        "selected conditional binding validation",
        &selected_validate,
    );
    assert_success(
        "selected conditional binding init",
        &run(&fixture.selected, &["init", "."]),
    );
    let selected_plan = run(&fixture.selected, &["plan", "--refresh=false", "."]);
    assert_success("selected conditional binding plan", &selected_plan);
    let selected_output = combined_output(&selected_plan);
    for binding in ["instance.a", "instance.inside", "instance.b"] {
        assert!(
            selected_output.contains(binding),
            "selected plan omitted {binding}:\n{selected_output}",
        );
    }

    let dropped_validate = run(&fixture.dropped, &["validate", "."]);
    assert_dropped_conditional_binding("dropped conditional binding validation", &dropped_validate);

    let dropped_plan = run(&fixture.dropped, &["plan", "--refresh=false", "."]);
    assert_dropped_conditional_binding("dropped conditional binding plan", &dropped_plan);
}

#[test]
fn plan_json_dependencies_are_derived_from_the_selected_value_branch() {
    let fixture = ConditionalDependencyFixture::new();
    for (project, expected) in [
        (&fixture.selected_argument, serde_json::json!([])),
        (&fixture.selected_reference, serde_json::json!(["on.a"])),
    ] {
        let plan = plan_json(project);
        let sorted = resource_dependency_bindings(&plan["sorted_resources"], "on.consumer");
        assert_eq!(sorted, &expected, "stale dependency in sorted resources");

        let creates = plan["plan"]["effects"]
            .as_array()
            .expect("plan effects")
            .iter()
            .filter_map(|effect| effect.get("Create"))
            .collect::<Vec<_>>();
        let create_resources = serde_json::Value::Array(creates.into_iter().cloned().collect());
        let effect = resource_dependency_bindings(&create_resources, "on.consumer");
        assert_eq!(effect, &expected, "stale dependency in create effect");
    }
}

#[test]
fn same_condition_guarded_references_validate_and_plan_for_both_branches() {
    let fixture = GuardedReferenceFixture::new();
    assert_success(
        "guarded-reference validation",
        &run(&fixture.caller, &["validate", "."]),
    );
    assert_success(
        "guarded-reference init",
        &run(&fixture.caller, &["init", "."]),
    );
    let plan = run(&fixture.caller, &["plan", "--refresh=false", "."]);
    assert_success("guarded-reference plan", &plan);
    let output = combined_output(&plan);
    assert!(
        output.contains("on.a") && output.contains("on.b") && output.contains("off.b"),
        "selected guarded bindings were missing from the plan:\n{output}",
    );
    assert!(
        !output.contains("off.a"),
        "the false instance retained its dropped binding:\n{output}",
    );
}
