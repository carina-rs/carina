//! CLI-level coverage for apply parallelism and dependency edge retention.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const DELAY_MS: u64 = 800;
const UPDATE_COUNT: usize = 13;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    max_active: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            max_active: project.join("max-active.txt"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self, body: &str) {
        fs::write(self.project.join("main.crn"), body).unwrap();
    }

    fn init(&self) {
        let output = carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("failed to execute carina init");
        assert_success("carina init", output);
    }

    fn apply(&self, parallelism: usize, delay_updates: bool) -> (Duration, usize) {
        let mut command = carina(&self.project);
        command.args([
            "apply",
            ".",
            "--auto-approve",
            "--lock=false",
            "--parallelism",
            &parallelism.to_string(),
        ]);
        command
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_MAX_ACTIVE_PATH", &self.max_active);
        if delay_updates {
            command.env("CARINA_MOCK_UPDATE_DELAY_MS", DELAY_MS.to_string());
        }

        let started = Instant::now();
        let output = command.output().expect("failed to execute carina apply");
        let elapsed = started.elapsed();
        assert_success("carina apply", output);
        let max_active = fs::read_to_string(&self.max_active)
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse::<usize>()
            .unwrap();
        (elapsed, max_active)
    }

    fn plan_out(&self, plan_path: &Path) {
        let output = carina(&self.project)
            .args(["plan", ".", "--out"])
            .arg(plan_path)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("failed to execute carina plan --out");
        assert_success("carina plan --out", output);
    }

    fn apply_plan(
        &self,
        plan_path: &Path,
        parallelism: usize,
        delay_updates: bool,
    ) -> (Duration, usize) {
        let mut command = carina(&self.project);
        command
            .arg("apply")
            .arg(plan_path)
            .args(["--auto-approve", "--lock=false", "--parallelism"])
            .arg(parallelism.to_string());
        command
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_MAX_ACTIVE_PATH", &self.max_active);
        if delay_updates {
            command.env("CARINA_MOCK_UPDATE_DELAY_MS", DELAY_MS.to_string());
        }

        let started = Instant::now();
        let output = command
            .output()
            .expect("failed to execute carina apply plan");
        let elapsed = started.elapsed();
        assert_success("carina apply plan", output);
        let max_active = fs::read_to_string(&self.max_active)
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse::<usize>()
            .unwrap();
        (elapsed, max_active)
    }
}

fn carina(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn assert_success(label: &str, output: std::process::Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn project_with_resources(resources: String) -> String {
    format!(
        r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

{resources}
"#
    )
}

fn independent_resources(version: &str) -> String {
    project_with_resources(
        (0..UPDATE_COUNT)
            .map(|idx| {
                format!(
                    r#"let r{idx} = mock.test.resource {{
  name = "r{idx}"
  tags = {{ version = "{version}" }}
}}
"#
                )
            })
            .collect(),
    )
}

fn parent_child_resources(version: &str, child_extra: impl Fn(usize) -> String) -> String {
    let mut resources = String::from(
        r#"let vpc = mock.test.resource {
  name = "vpc"
  tags = { version = ""#,
    );
    resources.push_str(version);
    resources.push_str(
        r#"" }
}
"#,
    );
    for idx in 0..12 {
        resources.push_str(&format!(
            r#"let child{idx} = mock.test.resource {{
  name = "child{idx}"
  tags = {{ version = "{version}" }}
{}
}}
"#,
            child_extra(idx)
        ));
    }
    project_with_resources(resources)
}

fn update_scenario(initial: String, updated: String, parallelism: usize) -> (Duration, usize) {
    let scenario = Scenario::new();
    scenario.write_config(&initial);
    scenario.init();
    scenario.apply(parallelism, false);
    scenario.write_config(&updated);
    scenario.apply(parallelism, true)
}

fn saved_plan_update_scenario(
    initial: String,
    updated: String,
    parallelism: usize,
) -> (Duration, usize) {
    let scenario = Scenario::new();
    scenario.write_config(&initial);
    scenario.init();
    scenario.apply(parallelism, false);
    scenario.write_config(&updated);
    let plan_path = scenario.project.join("plan.json");
    scenario.plan_out(&plan_path);
    scenario.apply_plan(&plan_path, parallelism, true)
}

#[test]
fn apply_parallelism_cli_e2e_covers_caps_and_unknown_update_edges() {
    let (cap_elapsed, cap_max) = update_scenario(
        independent_resources("old"),
        independent_resources("new"),
        4,
    );
    assert!(
        cap_max <= 4,
        "--parallelism 4 must cap in-flight updates at 4, got {cap_max}"
    );
    assert!(
        cap_max > 1,
        "--parallelism 4 should permit concurrent updates, got {cap_max}"
    );
    assert!(
        cap_elapsed >= Duration::from_millis(DELAY_MS * 3)
            && cap_elapsed < Duration::from_millis(DELAY_MS * UPDATE_COUNT as u64),
        "--parallelism 4 elapsed should land between parallel and serial bounds, got {cap_elapsed:?}"
    );

    let (serial_elapsed, serial_max) = update_scenario(
        independent_resources("old"),
        independent_resources("new"),
        1,
    );
    assert_eq!(
        serial_max, 1,
        "--parallelism 1 must run updates serially, got {serial_max}"
    );
    assert!(
        serial_elapsed >= Duration::from_millis(DELAY_MS * UPDATE_COUNT as u64),
        "--parallelism 1 elapsed should be at least the serial delay floor, got {serial_elapsed:?}"
    );

    let (bare_elapsed, bare_max) = update_scenario(
        parent_child_resources("old", |_| "  parent = vpc".to_string()),
        parent_child_resources("new", |_| "  parent = vpc".to_string()),
        8,
    );
    assert!(
        bare_max <= 8,
        "bare binding case must still respect --parallelism 8, got {bare_max}"
    );
    assert!(
        bare_elapsed >= Duration::from_millis(DELAY_MS * 2),
        "bare binding should still execute through the capped update scheduler, got {bare_elapsed:?}"
    );

    let (depends_elapsed, depends_max) = update_scenario(
        parent_child_resources("old", |_| "  directives { depends_on = [vpc] }".to_string()),
        parent_child_resources("new", |_| "  directives { depends_on = [vpc] }".to_string()),
        8,
    );
    assert!(
        depends_max <= 8,
        "depends_on case must still respect --parallelism 8, got {depends_max}"
    );
    assert!(
        depends_elapsed >= Duration::from_millis(DELAY_MS * 3),
        "depends_on should retain the parent gate instead of relaxing to two rounds, got {depends_elapsed:?}"
    );

    let (known_ref_elapsed, known_ref_max) = update_scenario(
        parent_child_resources("old", |_| "  parent_name = vpc.name".to_string()),
        parent_child_resources("new", |_| "  parent_name = vpc.name".to_string()),
        8,
    );
    assert!(
        known_ref_max <= 8,
        "known-ref case must still respect --parallelism 8, got {known_ref_max}"
    );
    assert!(
        known_ref_elapsed >= Duration::from_millis(DELAY_MS * 3 - 200),
        "known refs should retain the parent gate, got {known_ref_elapsed:?}"
    );
    assert!(
        known_ref_elapsed <= Duration::from_millis(DELAY_MS * 4),
        "known refs should still execute through the capped scheduler, got {known_ref_elapsed:?}"
    );
}

#[test]
fn apply_saved_plan_parallelism_keeps_known_refs_parent_gate() {
    let (depends_elapsed, depends_max) = saved_plan_update_scenario(
        parent_child_resources("old", |_| "  directives { depends_on = [vpc] }".to_string()),
        parent_child_resources("new", |_| "  directives { depends_on = [vpc] }".to_string()),
        8,
    );
    assert!(
        depends_max <= 8,
        "saved-plan depends_on case must respect --parallelism 8, got {depends_max}"
    );
    assert!(
        depends_elapsed >= Duration::from_millis(DELAY_MS * 3),
        "saved-plan depends_on should retain the parent gate, got {depends_elapsed:?}"
    );

    let (known_ref_elapsed, known_ref_max) = saved_plan_update_scenario(
        parent_child_resources("old", |_| "  parent_name = vpc.name".to_string()),
        parent_child_resources("new", |_| "  parent_name = vpc.name".to_string()),
        8,
    );
    assert!(
        known_ref_max <= 8,
        "saved-plan known-ref case must respect --parallelism 8, got {known_ref_max}"
    );
    assert!(
        known_ref_elapsed >= Duration::from_millis(DELAY_MS * 3 - 200),
        "saved-plan known refs should retain the parent gate, got {known_ref_elapsed:?}"
    );
    assert!(
        known_ref_elapsed <= Duration::from_millis(DELAY_MS * 4),
        "saved-plan known refs should still execute through the capped scheduler, got {known_ref_elapsed:?}"
    );
}
