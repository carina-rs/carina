//! End-to-end regressions for deferred replacement dependencies (#4174) and
//! deferred-for materialization diagnostics (#4160).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn new(main: &str) -> Self {
        let temp = tempfile::tempdir().expect("create deferred-for fixture");
        let project = temp.path().to_path_buf();
        fs::write(
            project.join("backend.crn"),
            r#"backend local { path = "carina.state.json" }
"#,
        )
        .expect("write backend configuration");
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(project.join("main.crn"), main).expect("write main configuration");

        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-provider-operations.log"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        self.carina_from(&self.project, args, &[])
    }

    fn carina_from(&self, cwd: &Path, args: &[&str], environment: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .envs(environment.iter().copied())
            .args(args)
            .output()
            .expect("run carina")
    }

    fn write_main(&self, main: &str) {
        fs::write(self.project.join("main.crn"), main).expect("rewrite main configuration");
    }

    fn apply_config(&self) -> Output {
        self.carina(&["apply", "--auto-approve", "--lock=false", "."])
    }

    fn apply_saved_plan(&self) -> Output {
        self.carina(&["apply", "p.plan", "--auto-approve", "--lock=false"])
    }

    fn save_plan(&self) -> Output {
        self.carina(&["plan", ".", "--out", "p.plan"])
    }

    fn state(&self) -> Value {
        read_json(&self.project.join("carina.state.json"))
    }

    fn operations(&self) -> Vec<String> {
        fs::read_to_string(&self.operation_log)
            .expect("read mock operation log")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn replacement_config(name: &str) -> String {
    format!(
        r#"let src = mock.test.resource {{ name = "{name}" }}
let rs = for _, t in (src.tags ?? {{ a = "x" }}) {{
  mock.test.resource {{ name = "off-${{t}}" }}
}}
"#,
    )
}

const ABSENT_ITERABLE_CONFIG: &str = r#"let src = mock.test.resource { name = "source" }
let rs = for _, t in src.tags {
  mock.test.resource { name = "off-${t}" }
}
"#;

fn assert_replacement_converged(scenario: &Scenario, apply: &Output) {
    assert_success("replacement apply", apply);

    let state = scenario.state();
    assert_eq!(resource_name(&state, "src"), "yy");
    assert_eq!(resource_name(&state, "rs.a"), "off-x");

    let operations = scenario.operations();
    let src_creates = positions(&operations, "create test.resource.src");
    let child_creates = positions(&operations, "create test.resource.rs.a");
    let child_deletes = positions(&operations, "delete test.resource.rs.a");
    let src_deletes = positions(&operations, "delete test.resource.src");
    assert_eq!(src_creates.len(), 2, "operations: {operations:?}");
    assert_eq!(child_creates.len(), 2, "operations: {operations:?}");
    assert_eq!(child_deletes.len(), 1, "operations: {operations:?}");
    assert_eq!(src_deletes.len(), 1, "operations: {operations:?}");
    assert!(
        src_creates[1] < child_creates[1]
            && child_creates[1] < child_deletes[0]
            && child_deletes[0] < src_deletes[0],
        "replacement must create src and the deferred child before deleting their old generations: {operations:?}",
    );
}

fn assert_absent_iterable_failure(output: &Output) {
    assert!(
        !output.status.success(),
        "deferred-for apply unexpectedly succeeded:\n{}",
        output_text(output),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deferred-for iterable `src.tags` is absent"),
        "stderr must retain the iterable-absent reason:\n{stderr}",
    );
    assert!(
        stderr.contains("binding `rs`") && stderr.contains("src.tags"),
        "stderr must identify the deferred target and source binding:\n{stderr}",
    );
    assert!(
        stderr.contains("main.crn:2"),
        "stderr must identify the deferred-for source location:\n{stderr}",
    );
    let final_report = stderr
        .split_once("Error: Apply failed.")
        .expect("final apply failure summary")
        .1;
    assert!(
        final_report.contains("deferred create")
            && final_report.contains("deferred materialization failure"),
        "final error must name the failed endpoint and kind: {final_report}"
    );
    assert_eq!(
        stderr
            .matches("deferred-for iterable `src.tags` is absent")
            .count(),
        1,
        "{stderr}"
    );
}

fn resource_name<'a>(state: &'a Value, identity: &str) -> &'a str {
    state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .find(|resource| resource["identity"] == identity)
        .unwrap_or_else(|| panic!("missing state resource {identity}: {state:#?}"))["current"]
        ["attributes"]["name"]
        .as_str()
        .unwrap_or_else(|| panic!("missing name for {identity}: {state:#?}"))
}

fn positions(operations: &[String], expected: &str) -> Vec<usize> {
    operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| (operation == expected).then_some(index))
        .collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn deferred_replace_dependency_direct_apply_converges() {
    let scenario = Scenario::new(&replacement_config("zz"));
    assert_success("initial apply", &scenario.apply_config());
    scenario.write_main(&replacement_config("yy"));

    let apply = scenario.apply_config();
    assert_replacement_converged(&scenario, &apply);
}

#[test]
fn deferred_replace_dependency_saved_plan_apply_converges() {
    let scenario = Scenario::new(&replacement_config("zz"));
    assert_success("initial apply", &scenario.apply_config());
    scenario.write_main(&replacement_config("yy"));
    assert_success("carina plan --out", &scenario.save_plan());

    let apply = scenario.apply_saved_plan();
    assert_replacement_converged(&scenario, &apply);
}

#[test]
fn deferred_for_absent_iterable_direct_apply_reports_location() {
    let scenario = Scenario::new(ABSENT_ITERABLE_CONFIG);
    assert_absent_iterable_failure(&scenario.apply_config());
}

#[test]
fn deferred_for_absent_iterable_saved_plan_apply_reports_location() {
    let scenario = Scenario::new(ABSENT_ITERABLE_CONFIG);
    assert_success("carina plan --out", &scenario.save_plan());
    assert_absent_iterable_failure(&scenario.apply_saved_plan());
}

#[test]
fn saved_deferred_diagnostic_is_project_relative_for_absolute_project_path() {
    assert_saved_diagnostic_from_another_cwd(false);
}

#[test]
fn saved_deferred_diagnostic_is_project_relative_for_relative_project_path() {
    assert_saved_diagnostic_from_another_cwd(true);
}

fn assert_saved_diagnostic_from_another_cwd(relative: bool) {
    let scenario = Scenario::new(ABSENT_ITERABLE_CONFIG);
    let plan_cwd = scenario.project.join("planning-cwd");
    let apply_cwd = scenario.project.join("applying-cwd");
    fs::create_dir(&plan_cwd).unwrap();
    fs::create_dir(&apply_cwd).unwrap();
    let project_arg = if relative {
        ".."
    } else {
        scenario.project.to_str().unwrap()
    };
    let plan_path = scenario.project.join("p.plan");
    assert_success(
        "save plan from another cwd",
        &scenario.carina_from(
            &plan_cwd,
            &["plan", project_arg, "--out", plan_path.to_str().unwrap()],
            &[],
        ),
    );
    let plan = read_json(&plan_path);
    let template = plan["plan"]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|effect| {
            effect
                .get("DeferredCreate")
                .map(|payload| &payload["template"])
        })
        .expect("saved deferred-create template");
    assert_eq!(template["file"], "main.crn");
    assert_eq!(template["line"], 2);
    let output = scenario.carina_from(
        &apply_cwd,
        &[
            "apply",
            plan_path.to_str().unwrap(),
            "--auto-approve",
            "--lock=false",
        ],
        &[],
    );
    assert_absent_iterable_failure(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("../main.crn") && !stderr.contains(scenario.project.to_str().unwrap()),
        "{stderr}"
    );
}

#[test]
fn destroy_retry_exhaustion_reports_distinctive_provider_reason() {
    // The mock's original provider error includes the identity. Put the AWS
    // retry classification and a distinctive blocker in it without adding a
    // second mock-only failure protocol.
    let scenario = Scenario::new(
        r#"let DependencyViolation_eni_4174_a = mock.test.resource { name = "a" }
let DependencyViolation_eni_4174_b = mock.test.resource { name = "b" }
"#,
    );
    assert_success("initial apply", &scenario.apply_config());
    let output = scenario.carina_from(
        &scenario.project,
        &[
            "destroy",
            ".",
            "--auto-approve",
            "--lock=false",
            "--refresh=false",
            "--parallelism",
            "1",
        ],
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "*")],
    );
    assert!(!output.status.success(), "{}", output_text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("retries exhausted after 4 attempts"),
        "{stderr}"
    );
    assert!(stderr.contains("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for test.resource.DependencyViolation_eni_4174_a"), "{stderr}");
    assert!(
        stderr.contains("delete mock.test.resource.DependencyViolation_eni_4174_a: provider error"),
        "{stderr}"
    );
    assert_eq!(
        scenario.state()["resources"].as_array().unwrap().len(),
        2,
        "failed deletes must remain in state"
    );
}

#[test]
fn destroy_stalled_retry_keeps_provider_reason_and_skips_its_dependency() {
    let scenario = Scenario::new(
        r#"let producer = mock.test.resource { name = "producer" }
let DependencyViolation_eni_4174 = mock.test.resource {
  name = "consumer"
  parent = producer.identifier
}

"#,
    );
    assert_success("initial apply", &scenario.apply_config());
    let output = scenario.carina_from(
        &scenario.project,
        &[
            "destroy",
            ".",
            "--auto-approve",
            "--lock=false",
            "--refresh=false",
            "--parallelism",
            "1",
        ],
        &[(
            "CARINA_MOCK_DELETE_FAIL_FOR",
            "test.resource.DependencyViolation_eni_4174",
        )],
    );
    assert!(!output.status.success(), "{}", output_text(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed (no retry possible: no other deletes can make progress)"),
        "{stderr}"
    );
    assert!(stderr.contains("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for test.resource.DependencyViolation_eni_4174"), "{stderr}");
    assert!(stderr.contains("1 failed, 1 skipped"), "{stderr}");
    assert_eq!(scenario.state()["resources"].as_array().unwrap().len(), 2);
}

#[cfg(unix)]
#[test]
fn destroy_graceful_cancellation_keeps_pending_retry_failure() {
    assert_cancelled_retry_keeps_provider_cause(false);
}

#[cfg(unix)]
#[test]
fn destroy_cleanup_priority_keeps_pending_retry_failure() {
    assert_cancelled_retry_keeps_provider_cause(true);
}

#[cfg(unix)]
fn assert_cancelled_retry_keeps_provider_cause(cleanup_priority: bool) {
    use std::time::{Duration, Instant};

    struct KillOnDrop(std::process::Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let scenario = Scenario::new(
        r#"let a_DependencyViolation_eni_4174 = mock.test.resource { name = "failed" }
let b_draining = mock.test.resource { name = "draining" }
let c_never_ran = mock.test.resource { name = "never-ran" }
"#,
    );
    assert_success("initial apply", &scenario.apply_config());
    let ready = scenario.project.join("delete-ready");
    let stdout_path = scenario.project.join("destroy.stdout");
    let stderr_path = scenario.project.join("destroy.stderr");
    let mut child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&scenario.project)
            .args([
                "destroy",
                ".",
                "--auto-approve",
                "--lock=false",
                "--refresh=false",
                "--parallelism",
                "1",
            ])
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &scenario.mock_state)
            .env(
                "CARINA_MOCK_DELETE_FAIL_FOR",
                "test.resource.a_DependencyViolation_eni_4174",
            )
            .env(
                "CARINA_MOCK_DELETE_DELAY_MS_FOR",
                "test.resource.b_draining",
            )
            .env(
                "CARINA_MOCK_DELETE_DELAY_MS",
                if cleanup_priority { "60000" } else { "1500" },
            )
            .env("CARINA_MOCK_DELETE_READY_PATH", &ready)
            .stdout(fs::File::create(&stdout_path).unwrap())
            .stderr(fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(
            Instant::now() < deadline,
            "delete did not reach readiness handshake"
        );
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "destroy exited before draining"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The second delete can only start after the first provider failure was
    // retained for retry. The signal therefore cannot precede that failure.
    assert_eq!(
        unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGINT) },
        0
    );
    if cleanup_priority {
        while !fs::read_to_string(&stderr_path)
            .unwrap()
            .contains("Received shutdown signal: Interrupt.")
        {
            assert!(
                Instant::now() < deadline,
                "graceful shutdown was not observed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            unsafe { libc::kill(child.0.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
    }
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "destroy did not finish cancellation"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read_to_string(stdout_path).unwrap();
    let stderr = fs::read_to_string(stderr_path).unwrap();
    assert_eq!(
        status.code(),
        Some(130),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("destroy cancelled before retry"),
        "{stderr}"
    );
    assert!(stderr.contains("CARINA_MOCK_DELETE_FAIL_FOR requested delete failure for test.resource.a_DependencyViolation_eni_4174"), "{stderr}");
    assert!(
        stdout.contains("1 failed, 1 skipped"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        scenario.state()["resources"].as_array().unwrap().len(),
        if cleanup_priority { 3 } else { 2 }
    );
}

#[test]
fn deferred_template_create_only_reference_is_replaced_through_deferred_replace() {
    fn config(name: &str) -> String {
        format!(
            r#"let src = mock.test.resource {{ name = "{name}" }}
let shape = mock.test.resource {{ name = "shape-{name}" }}
let rs = for _, t in (shape.tags ?? {{ a = "x" }}) {{
  mock.test.resource {{ name = "off-${{src.name}}-${{t}}" }}
}}
"#
        )
    }
    let scenario = Scenario::new(&config("zz"));
    assert_success("initial template-reference apply", &scenario.apply_config());
    assert_eq!(resource_name(&scenario.state(), "rs.a"), "off-zz-x");
    scenario.write_main(&config("yy"));
    assert_success("save template-reference plan", &scenario.save_plan());
    let plan = read_json(&scenario.project.join("p.plan"));
    assert!(
        plan["plan"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| effect.get("DeferredReplace").is_some()),
        "deferred children must be replaced: {plan}"
    );
    assert_success(
        "apply template-reference plan",
        &scenario.apply_saved_plan(),
    );
    assert_eq!(resource_name(&scenario.state(), "rs.a"), "off-yy-x");
    assert_eq!(
        positions(&scenario.operations(), "create test.resource.rs.a").len(),
        2
    );
    assert_eq!(
        positions(&scenario.operations(), "delete test.resource.rs.a").len(),
        1
    );
}
