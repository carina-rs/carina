use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "fn_declared_checks_output/identity_migration.rs"]
mod identity_migration;

#[tokio::test]
async fn primitive_module_identity_upgrade_can_be_selected_with_moved() {
    const CHILD: &str = "CARINA_FUNCTION_IDENTITY_MIGRATION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        identity_migration::check_legacy_rows().await;
        return;
    }

    // Capture the real plan renderer in a child, without redirecting this
    // process's output or changing its environment or working directory.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "primitive_module_identity_upgrade_can_be_selected_with_moved",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan = stdout
        .split_once("MUTABLE_MIGRATION_PLAN_BEGIN\n")
        .unwrap()
        .1
        .split_once("MUTABLE_MIGRATION_PLAN_END")
        .unwrap()
        .0;
    assert!(plan.contains("Plan: 1 to add, 0 to change, 1 to destroy."));
    assert!(plan.contains("+ test.Sink instance.test_sink_2190428c0824b060"));
    assert!(plan.contains("- test.Sink instance.test_sink_22c482a506328080"));

    for kind in ["IdentitySink", "Sink"] {
        let moved_plan = stdout
            .split_once(&format!("{kind}_MOVED_PLAN_BEGIN\n"))
            .unwrap()
            .1
            .split_once(&format!("{kind}_MOVED_PLAN_END"))
            .unwrap()
            .0;
        assert_eq!(
            moved_plan.lines().find(|line| line.starts_with("Plan:")),
            Some("Plan: 0 to add, 0 to change, 0 to destroy, 1 to move.")
        );
        let converged_plan = stdout
            .split_once(&format!("{kind}_CONVERGED_PLAN_BEGIN\n"))
            .unwrap()
            .1
            .split_once(&format!("{kind}_CONVERGED_PLAN_END"))
            .unwrap()
            .0;
        assert_eq!(
            converged_plan
                .lines()
                .find(|line| line.starts_with("No changes.")),
            Some("No changes. Infrastructure is up-to-date.")
        );
    }
}

const MARKER: &str = "declared-secret-cobalt-784531";

enum ArtifactPresence {
    Required,
    Absent,
}
struct Project(tempfile::TempDir);
impl Project {
    fn new(main: &str, child: Option<&str>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("declared checks ' ")
            .tempdir()
            .unwrap();
        std::fs::write(root.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            root.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("main.crn"), "").unwrap();
        let project = Self(root);
        project.success(&["init"]);
        std::fs::write(project.path().join("main.crn"), main).unwrap();
        if let Some(child) = child {
            std::fs::create_dir(project.path().join("child")).unwrap();
            std::fs::write(project.path().join("child/main.crn"), child).unwrap();
        }
        project
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(args)
            .arg(self.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.path().join("mock-provider-state.json"),
            )
            .env("CARINA_MOCK_OP_LOG", self.path().join("operations.log"))
            .output()
            .unwrap();
        for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            assert!(
                !String::from_utf8_lossy(bytes).contains(MARKER),
                "secret in command output: args={args:?}, stream={stream}"
            );
        }
        output
    }
    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn assert_private_artifact(&self, path: &Path, presence: ArtifactPresence) {
        match presence {
            ArtifactPresence::Required => {
                assert!(path.is_file(), "missing required artifact");
                assert!(
                    !std::fs::read_to_string(path).unwrap().contains(MARKER),
                    "secret in artifact"
                );
            }
            ArtifactPresence::Absent => assert!(!path.exists(), "unexpected saved artifact"),
        }
    }
}

fn primitive_lookup_project(function: &str, expression: &str, module: bool) -> Project {
    let body = format!(
        "let src = mock.test.resource {{ name = 'src' }}\n{function}\nlet sink = mock.test.resource {{ name = 'sink' computed = {expression} }}\n"
    );
    if module {
        Project::new(
            "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
            Some(&format!("arguments {{ label: String }}\n{body}")),
        )
    } else {
        Project::new(&body, None)
    }
}

fn primitive_sink_values(project: &Project, module: bool) -> Vec<serde_json::Value> {
    let path = project.path().join("mock-provider-state.json");
    if !path.exists() {
        return Vec::new();
    }
    let store: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let key = if module {
        "test.resource.instance.sink"
    } else {
        "test.resource.sink"
    };
    store["states"][key]
        .get("computed")
        .cloned()
        .into_iter()
        .collect()
}

fn assert_primitive_lookup_rejected(module: bool, return_annotation: bool) {
    let (function, expression, diagnostic) = if return_annotation {
        (
            "fn h(x): Int { x }",
            "\"n-${h(lookup({ 'src-id' = 'text' }, src.identifier, 0))}\"",
            "function 'h': return value: expected Int, got string 'text'",
        )
    } else {
        (
            "fn g(x: Int): String { \"n-${x}\" }",
            "g(lookup({ 'src-id' = 'text' }, src.identifier, 0))",
            "function 'g': parameter 'x': expected Int, got string 'text'",
        )
    };
    let project = primitive_lookup_project(function, expression, module);
    project.success(&["plan"]);
    let output = project.run(&["apply", "--auto-approve", "--parallelism=1"]);
    let values = primitive_sink_values(&project, module);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={}\nstderr={}\nsink_values={values:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let expected = if return_annotation {
        let file = project
            .path()
            .join(if module { "child/main.crn" } else { "main.crn" });
        let line = if module { 3 } else { 2 };
        format!(
            "→ {}: Invalid expression at line {line}: {diagnostic}",
            file.display()
        )
    } else {
        let line = if module { 4 } else { 3 };
        format!("→ Invalid expression at line {line}: {diagnostic}")
    };
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .any(|actual| actual.trim_start() == expected),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(values.is_empty(), "rejected calls must not reach the sink");
    let operations = std::fs::read_to_string(project.path().join("operations.log")).unwrap();
    let sink = if module {
        "test.resource.instance.sink"
    } else {
        "test.resource.sink"
    };
    assert!(
        !operations
            .lines()
            .any(|operation| operation == format!("create {sink}"))
    );
}

#[test]
fn primitive_parameter_lookup_is_checked_at_apply_root() {
    assert_primitive_lookup_rejected(false, false);
}

#[test]
fn primitive_parameter_lookup_is_checked_at_apply_module() {
    assert_primitive_lookup_rejected(true, false);
}

#[test]
fn primitive_return_lookup_is_checked_at_apply_root() {
    assert_primitive_lookup_rejected(false, true);
}

#[test]
fn primitive_return_lookup_is_checked_at_apply_module() {
    assert_primitive_lookup_rejected(true, true);
}

fn assert_valid_primitive_lookup(module: bool) {
    let project = primitive_lookup_project(
        "fn g(x: String): String { \"n-${x}\" }",
        "g(lookup({ 'src-id' = 'text' }, src.identifier, 0))",
        module,
    );
    let plan = project.success(&["plan"]);
    assert!(String::from_utf8_lossy(&plan.stdout).contains("g(lookup("));
    project.success(&["apply", "--auto-approve", "--parallelism=1"]);
    assert_eq!(
        primitive_sink_values(&project, module),
        [serde_json::json!("n-text")]
    );
}

#[test]
fn primitive_string_lookup_retains_then_dispatches_root() {
    assert_valid_primitive_lookup(false);
}

#[test]
fn primitive_string_lookup_retains_then_dispatches_module() {
    assert_valid_primitive_lookup(true);
}

fn assert_known_handle_primitive_return_inlines(module: bool) {
    let project = primitive_lookup_project(
        "fn g(r: mock.test.resource): String { \"prefix-${r.computed}\" }",
        "g(src)",
        module,
    );
    let saved = project.path().join("known-handle.plan.json");
    project.success(&["plan", "--out", saved.to_str().unwrap()]);
    project.assert_private_artifact(&saved, ArtifactPresence::Required);
}

#[test]
fn known_handle_primitive_return_inlines_root_plan_out() {
    assert_known_handle_primitive_return_inlines(false);
}

#[test]
fn known_handle_primitive_return_inlines_module_plan_out() {
    assert_known_handle_primitive_return_inlines(true);
}

fn assert_primitive_saved_refusal(module: bool, json: bool) {
    let project = primitive_lookup_project(
        "fn g(x: String): String { \"n-${x}\" }",
        "g(lookup({ 'src-id' = 'text' }, src.identifier, 0))",
        module,
    );
    let saved = project.path().join("saved.plan.json");
    let args = if json {
        vec!["plan", "--refresh=false", "--json"]
    } else {
        vec!["plan", "--refresh=false", "--out", saved.to_str().unwrap()]
    };
    let output = project.run(&args);
    assert_eq!(output.status.code(), Some(1));
    let expected = format!(
        "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.\n{}Error: Cannot save plan: the plan contains an unevaluated call to the user-defined function 'g'. A saved plan cannot carry this call.\nError: Run `{}` directly on the project, without a saved plan.\n",
        if json { "Using mock provider\n" } else { "" },
        carina_core::hint::ProjectCommand::new("apply", project.path()),
    );
    assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
    if json {
        assert!(output.stdout.is_empty());
    }
    project.assert_private_artifact(&saved, ArtifactPresence::Absent);
}

#[test]
fn primitive_saved_refusal_root_plan_out() {
    assert_primitive_saved_refusal(false, false);
}

#[test]
fn primitive_saved_refusal_module_plan_out() {
    assert_primitive_saved_refusal(true, false);
}

#[test]
fn primitive_saved_refusal_root_json() {
    assert_primitive_saved_refusal(false, true);
}

#[test]
fn primitive_saved_refusal_module_json() {
    assert_primitive_saved_refusal(true, true);
}

fn hashes(project: &Project) -> Vec<serde_json::Value> {
    let path = project.path().join("carina.state.json");
    project.assert_private_artifact(&path, ArtifactPresence::Required);
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut values = Vec::new();
    for resource in state["resources"].as_array().unwrap() {
        let value = resource["current"]["attributes"]["computed"].clone();
        assert!(value["$secret"].is_string());
        values.push(value);
    }
    assert_eq!(values.len(), 2);
    values
}

#[test]
fn inlined_function_secret_output_stays_private_and_hashed() {
    // A known resource handle permits this primitive-return function to inline.
    for module in [false, true] {
        let body = format!(
            "let src = mock.test.resource {{ name = 'src' computed = secret('{MARKER}') }}\nfn g(r: mock.test.resource): String {{ \"prefix-${{r.computed}}\" }}\nmock.test.resource {{ name = 'sink' computed = g(src) }}\n"
        );
        let project = if module {
            Project::new(
                "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
                Some(&format!("arguments {{ label: String }}\n{body}")),
            )
        } else {
            Project::new(&body, None)
        };
        project.success(&["apply", "--auto-approve"]);
        let before = hashes(&project);
        project.success(&["plan"]);
        let saved: PathBuf = project.path().join("saved.plan.json");
        project.success(&["plan", "--out", saved.to_str().unwrap()]);
        project.assert_private_artifact(&saved, ArtifactPresence::Required);
        let output = project.success(&["state", "refresh"]);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("State refreshed: 0 resources updated, 2 resources unchanged."));
        assert!(!stdout.contains("computed:"));
        assert_eq!(hashes(&project), before);
    }
}

fn typed_project(body: &str, module: bool) -> Project {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let wasm = ["carina-provider-mock.wasm", "carina_provider_mock.wasm"]
        .into_iter()
        .map(|name| target.join("wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("the mock WASM fixture must be built");
    let project = Project::new("", None);
    for destination in [
        project.path().join("local.wasm"),
        project
            .path()
            .join(".carina/providers/file/local/local.wasm"),
    ] {
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(&wasm, destination).unwrap();
    }
    std::fs::write(
        project.path().join("providers.crn"),
        "provider mock { source = 'file://local.wasm' }\n",
    )
    .unwrap();
    if module {
        std::fs::write(
            project.path().join("main.crn"),
            "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
        )
        .unwrap();
        std::fs::create_dir(project.path().join("child")).unwrap();
        std::fs::write(
            project.path().join("child/main.crn"),
            format!("arguments {{ label: String }}\n{body}"),
        )
        .unwrap();
    } else {
        std::fs::write(project.path().join("main.crn"), body).unwrap();
    }
    project
}

fn assert_binary_refusal(project: &Project, json: bool, problem: &str) {
    let saved = project.path().join("saved.plan.json");
    let args = if json {
        vec!["plan", "--refresh=false", "--json"]
    } else {
        vec!["plan", "--refresh=false", "--out", saved.to_str().unwrap()]
    };
    {
        let expected = format!(
            "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.\n{}Error: Cannot save plan: {}\nError: Run `{}` directly on the project, without a saved plan.\n",
            if json {
                "Using mock (instance=default, source: file://local.wasm)\n"
            } else {
                ""
            },
            problem,
            carina_core::hint::ProjectCommand::new("apply", project.path()),
        );
        let output = project.run(&args);
        assert_eq!(output.status.code(), Some(1));
        if json {
            assert!(
                output.stdout.is_empty(),
                "refused JSON command must not print a document"
            );
        }
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
        project.assert_private_artifact(&saved, ArtifactPresence::Absent);
    }
}

fn assert_binary_retained_refusal(json: bool) {
    for module in [false, true] {
        let project = typed_project(
            "let src = mock.test.read_back { present = 'allowed' }\nfn checked(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nlet sink = mock.test.read_back { present = checked(src) }\n",
            module,
        );
        assert_binary_refusal(
            &project,
            json,
            "the plan contains an unevaluated call to the user-defined function 'checked'. A saved plan cannot carry this call.",
        );
    }
}

fn assert_distinct_function_names(json: bool) {
    for module in [false, true] {
        let project = typed_project(
            "let src = mock.test.read_back { present = 'allowed' }\nfn zeta(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nfn alpha(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nlet first = mock.test.read_back { present = zeta(src) }\nlet second = mock.test.read_back { present = alpha(src) }\nlet third = mock.test.read_back { present = zeta(src) }\n",
            module,
        );
        assert_binary_refusal(
            &project,
            json,
            "the plan contains unevaluated calls to the user-defined functions 'alpha', 'zeta'. A saved plan cannot carry these calls.",
        );
    }
}
#[test]
fn retained_calls_report_distinct_function_names_in_order_plan_out() {
    assert_distinct_function_names(false);
}
#[test]
fn retained_calls_report_distinct_function_names_in_order_json() {
    assert_distinct_function_names(true);
}

#[test]
fn retained_calls_refuse_saved_output_through_the_binary_json() {
    assert_binary_retained_refusal(true);
}
#[test]
fn retained_calls_refuse_saved_output_through_the_binary_plan_out() {
    assert_binary_retained_refusal(false);
}

#[test]
#[should_panic(expected = "missing required artifact")]
fn private_artifact_requires_the_expected_file() {
    let project = Project::new("", None);
    project.assert_private_artifact(
        &project.path().join("missing.plan.json"),
        ArtifactPresence::Required,
    );
}

#[test]
fn stale_state_value_checks_match_at_root_and_in_module() {
    for module in [false, true] {
        let source = "let src = mock.test.read_back { present = 'before' create_read_back_pending_polls = 1 }\n";
        let project = typed_project(source, module);
        project.success(&["apply", "--auto-approve"]);
        let body = "let src = mock.test.read_back { present = 'after' create_read_back_pending_polls = 1 }\nfn checked(x: mock.test.sdk_validator.Guarded | 'ready'): String { x }\nlet sink = mock.test.read_back { present = checked(src.computed) }\n";
        std::fs::write(
            project
                .path()
                .join(if module { "child/main.crn" } else { "main.crn" }),
            if module {
                format!("arguments {{ label: String }}\n{body}")
            } else {
                body.into()
            },
        )
        .unwrap();
        project.success(&["plan", "--refresh=false"]);
        let path = project.path().join("carina.state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rows = state["resources"].as_array_mut().unwrap();
        assert_eq!(rows.len(), 1);
        rows[0]["current"]["attributes"]["computed"] = "bad-name".into();
        let before = serde_json::to_vec(&state).unwrap();
        std::fs::write(&path, &before).unwrap();
        let output = project.run(&["plan", "--refresh=false"]);
        assert_eq!(output.status.code(), Some(1));
        let line = if module { 4 } else { 3 };
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.\nError: Invalid expression at line {line}: function 'checked': parameter 'x': expected mock.test.sdk_validator.Guarded | 'ready', got string 'bad-name'\n"
            )
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        project.success(&["state", "refresh"]);
        project.success(&["plan", "--refresh=false"]);
    }
}
