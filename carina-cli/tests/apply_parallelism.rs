//! CLI-level coverage for apply parallelism and update-update edge retention.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const UPDATE_COUNT: usize = 13;

#[derive(Debug)]
enum UpdateTraceEvent {
    Start { resource: String, active: usize },
    Finish { resource: String },
}

#[derive(Debug)]
struct UpdateRun {
    max_active: usize,
    trace: Vec<UpdateTraceEvent>,
}

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    max_active: PathBuf,
    update_trace: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            max_active: project.join("max-active.txt"),
            update_trace: project.join("update-trace.txt"),
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

    fn apply(&self, parallelism: usize) -> UpdateRun {
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
            .env("CARINA_MOCK_MAX_ACTIVE_PATH", &self.max_active)
            .env("CARINA_MOCK_UPDATE_TRACE_PATH", &self.update_trace);

        let output = command.output().expect("failed to execute carina apply");
        assert_success("carina apply", output);
        self.read_update_run()
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

    fn apply_plan(&self, plan_path: &Path, parallelism: usize) -> UpdateRun {
        let mut command = carina(&self.project);
        command
            .arg("apply")
            .arg(plan_path)
            .args(["--auto-approve", "--lock=false", "--parallelism"])
            .arg(parallelism.to_string());
        command
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_MAX_ACTIVE_PATH", &self.max_active)
            .env("CARINA_MOCK_UPDATE_TRACE_PATH", &self.update_trace);

        let output = command
            .output()
            .expect("failed to execute carina apply plan");
        assert_success("carina apply plan", output);
        self.read_update_run()
    }

    fn read_update_run(&self) -> UpdateRun {
        let max_active = fs::read_to_string(&self.max_active)
            .unwrap_or_else(|_| "0".to_string())
            .trim()
            .parse::<usize>()
            .unwrap();
        let trace = fs::read_to_string(&self.update_trace)
            .unwrap_or_else(|err| panic!("failed to read update trace: {err}"))
            .lines()
            .map(parse_update_trace_event)
            .collect();
        UpdateRun { max_active, trace }
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

fn parse_update_trace_event(line: &str) -> UpdateTraceEvent {
    let mut fields = line.split_whitespace();
    let event = fields.next();
    let resource = fields.next().map(str::to_string);
    let active = fields.next();
    let trailing = fields.next();
    match (event, resource, active, trailing) {
        (Some("start"), Some(resource), Some(active), None) => UpdateTraceEvent::Start {
            resource,
            active: active
                .parse()
                .unwrap_or_else(|err| panic!("invalid active count in trace line {line:?}: {err}")),
        },
        (Some("finish"), Some(resource), None, None) => UpdateTraceEvent::Finish { resource },
        _ => panic!("invalid update trace line: {line:?}"),
    }
}

fn assert_complete_update_trace(label: &str, trace: &[UpdateTraceEvent]) {
    let mut started = trace
        .iter()
        .filter_map(|event| match event {
            UpdateTraceEvent::Start { resource, .. } => Some(resource.as_str()),
            UpdateTraceEvent::Finish { .. } => None,
        })
        .collect::<Vec<_>>();
    let mut finished = trace
        .iter()
        .filter_map(|event| match event {
            UpdateTraceEvent::Start { .. } => None,
            UpdateTraceEvent::Finish { resource } => Some(resource.as_str()),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        UPDATE_COUNT,
        "{label} must start every update; trace={trace:#?}"
    );
    started.sort_unstable();
    finished.sort_unstable();
    assert_eq!(
        finished, started,
        "{label} must finish every update it starts; trace={trace:#?}"
    );
}

fn update_waves(trace: &[UpdateTraceEvent]) -> Vec<Vec<&str>> {
    let mut waves = Vec::<Vec<&str>>::new();
    let mut started_in_wave = HashMap::<&str, usize>::new();
    let mut active_resources = HashSet::<&str>::new();
    let mut finished_resources = HashSet::<&str>::new();
    let mut latest_finished_wave = None::<usize>;

    for event in trace {
        match event {
            UpdateTraceEvent::Start { resource, active } => {
                let resource = resource.as_str();
                assert!(
                    !started_in_wave.contains_key(resource),
                    "update trace starts {resource} more than once; trace={trace:#?}"
                );
                assert_eq!(
                    *active,
                    active_resources.len() + 1,
                    "update trace has an inconsistent active count at {resource}; trace={trace:#?}"
                );
                assert!(
                    active_resources.insert(resource),
                    "update trace starts active resource {resource} again; trace={trace:#?}"
                );

                // A start is one causal wave after the newest wave with an observed
                // finish. The mock's yield makes a ready group record all starts
                // before its first finish, so this preserves narrower later waves
                // and turns a serial start/finish trace into one wave per update.
                let wave = latest_finished_wave.map_or(0, |finished| finished + 1);
                if waves.len() == wave {
                    waves.push(Vec::new());
                }
                assert!(
                    wave < waves.len(),
                    "update trace skipped a wave before starting {resource}; trace={trace:#?}"
                );
                waves[wave].push(resource);
                started_in_wave.insert(resource, wave);
            }
            UpdateTraceEvent::Finish { resource } => {
                let resource = resource.as_str();
                let wave = *started_in_wave.get(resource).unwrap_or_else(|| {
                    panic!("update trace finishes {resource} before it starts; trace={trace:#?}")
                });
                assert!(
                    active_resources.remove(resource),
                    "update trace finishes inactive resource {resource}; trace={trace:#?}"
                );
                assert!(
                    finished_resources.insert(resource),
                    "update trace finishes {resource} more than once; trace={trace:#?}"
                );
                latest_finished_wave =
                    Some(latest_finished_wave.map_or(wave, |finished| finished.max(wave)));
            }
        }
    }

    assert_eq!(
        active_resources,
        HashSet::new(),
        "update trace leaves resources active; trace={trace:#?}"
    );
    waves
}

fn assert_parallelism_cap(label: &str, run: &UpdateRun, limit: usize) {
    assert!(
        run.max_active <= limit,
        "{label} must respect --parallelism {limit}, got {}",
        run.max_active
    );
    let traced_max = run
        .trace
        .iter()
        .filter_map(|event| match event {
            UpdateTraceEvent::Start { active, .. } => Some(*active),
            UpdateTraceEvent::Finish { .. } => None,
        })
        .max()
        .unwrap_or(0);
    assert_eq!(
        run.max_active, traced_max,
        "{label} max-active counter must match its trace; trace={:#?}",
        run.trace
    );
}

fn assert_update_schedule(
    label: &str,
    run: &UpdateRun,
    parallelism: usize,
    expected_wave_widths: &[usize],
) {
    assert_parallelism_cap(label, run, parallelism);
    assert_complete_update_trace(label, &run.trace);
    let waves = update_waves(&run.trace);
    let actual_wave_widths = waves.iter().map(Vec::len).collect::<Vec<_>>();
    assert_eq!(
        actual_wave_widths, expected_wave_widths,
        "{label} must have update wave widths {expected_wave_widths:?}; waves={waves:#?}; trace={:#?}",
        run.trace
    );
}

fn assert_known_refs_bypass_parent_gate(
    label: &str,
    known_ref_run: &UpdateRun,
    depends_run: &UpdateRun,
) {
    let known_ref_waves = update_waves(&known_ref_run.trace);
    let depends_waves = update_waves(&depends_run.trace);
    assert_eq!(
        depends_waves[0],
        ["test.resource.vpc"],
        "{label} depends_on must gate children on the parent; waves={depends_waves:#?}"
    );
    assert!(
        known_ref_waves[0].contains(&"test.resource.vpc") && known_ref_waves[0].len() > 1,
        "{label} known-disjoint refs must let children update alongside the parent; waves={known_ref_waves:#?}"
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

fn update_scenario(initial: String, updated: String, parallelism: usize) -> UpdateRun {
    let scenario = Scenario::new();
    scenario.write_config(&initial);
    scenario.init();
    scenario.apply(parallelism);
    scenario.write_config(&updated);
    scenario.apply(parallelism)
}

fn saved_plan_update_scenario(initial: String, updated: String, parallelism: usize) -> UpdateRun {
    let scenario = Scenario::new();
    scenario.write_config(&initial);
    scenario.init();
    scenario.apply(parallelism);
    scenario.write_config(&updated);
    let plan_path = scenario.project.join("plan.json");
    scenario.plan_out(&plan_path);
    scenario.apply_plan(&plan_path, parallelism)
}

#[test]
fn apply_parallelism_cli_e2e_covers_caps_and_unknown_update_edges() {
    let cap = update_scenario(
        independent_resources("old"),
        independent_resources("new"),
        4,
    );
    assert_update_schedule("--parallelism 4", &cap, 4, &[4, 4, 4, 1]);

    let serial = update_scenario(
        independent_resources("old"),
        independent_resources("new"),
        1,
    );
    assert_update_schedule("--parallelism 1", &serial, 1, &[1; UPDATE_COUNT]);

    let bare = update_scenario(
        parent_child_resources("old", |_| "  parent = vpc".to_string()),
        parent_child_resources("new", |_| "  parent = vpc".to_string()),
        8,
    );
    assert_update_schedule("bare binding", &bare, 8, &[8, 5]);

    let depends = update_scenario(
        parent_child_resources("old", |_| "  directives { depends_on = [vpc] }".to_string()),
        parent_child_resources("new", |_| "  directives { depends_on = [vpc] }".to_string()),
        8,
    );
    assert_update_schedule("depends_on", &depends, 8, &[1, 8, 4]);

    let known_ref = update_scenario(
        parent_child_resources("old", |_| "  parent_name = vpc.name".to_string()),
        parent_child_resources("new", |_| "  parent_name = vpc.name".to_string()),
        8,
    );
    assert_update_schedule("known-disjoint refs", &known_ref, 8, &[8, 5]);
    assert_known_refs_bypass_parent_gate("direct apply", &known_ref, &depends);
}

#[test]
fn apply_saved_plan_parallelism_relaxes_known_disjoint_refs() {
    let depends = saved_plan_update_scenario(
        parent_child_resources("old", |_| "  directives { depends_on = [vpc] }".to_string()),
        parent_child_resources("new", |_| "  directives { depends_on = [vpc] }".to_string()),
        8,
    );
    assert_update_schedule("saved-plan depends_on", &depends, 8, &[1, 8, 4]);

    let known_ref = saved_plan_update_scenario(
        parent_child_resources("old", |_| "  parent_name = vpc.name".to_string()),
        parent_child_resources("new", |_| "  parent_name = vpc.name".to_string()),
        8,
    );
    assert_update_schedule("saved-plan known-disjoint refs", &known_ref, 8, &[8, 5]);
    assert_known_refs_bypass_parent_gate("saved-plan apply", &known_ref, &depends);
}
