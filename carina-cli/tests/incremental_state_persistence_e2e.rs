#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const FIRST: &str = "a_completed";
const BLOCKED: &str = "z_blocked";

#[test]
fn apply_persists_a_completed_create_before_sigkill() {
    let fixture = ProjectFixture::new();
    let ready = fixture.project.join("blocked-create-ready");
    let mut child = fixture.spawn(
        &["apply", ".", "--auto-approve", "--parallelism", "1"],
        &[
            ("CARINA_MOCK_CREATE_DELAY_MS_FOR", "test.resource.z_blocked"),
            ("CARINA_MOCK_CREATE_DELAY_MS", "60000"),
            ("CARINA_MOCK_CREATE_READY_PATH", ready.to_str().unwrap()),
        ],
    );

    wait_for_file(&ready, Duration::from_secs(10));
    wait_for_state(&fixture.state, Duration::from_secs(5), |state| {
        state
            .find_resource("mock", "test.resource", FIRST)
            .is_some()
    });

    child.kill_with_sigkill();

    let state = load_state(&fixture.state);
    let completed = state
        .find_resource("mock", "test.resource", FIRST)
        .expect("the completed create must be durable before the run ends");
    assert_eq!(
        completed.identifier().map(|id| id.as_str()),
        Some("mock-id")
    );
    assert!(
        state
            .find_resource("mock", "test.resource", BLOCKED)
            .is_none(),
        "the provider operation that never returned must not be persisted"
    );

    let plan = fixture.output(&["plan", "."], &[]);
    assert!(plan.status.success(), "plan failed: {}", output_text(&plan));
    let stdout = String::from_utf8_lossy(&plan.stdout);
    assert!(
        stdout.contains("Plan: 1 to add, 0 to change, 0 to destroy."),
        "the follow-up plan must create only the unfinished resource:\n{stdout}"
    );
}

#[test]
fn destroy_persists_a_completed_delete_before_sigkill() {
    let fixture = ProjectFixture::new();
    let initial = fixture.output(&["apply", ".", "--auto-approve"], &[]);
    assert!(
        initial.status.success(),
        "fixture apply failed: {}",
        output_text(&initial)
    );

    let ready = fixture.project.join("blocked-delete-ready");
    let mut child = fixture.spawn(
        &["destroy", ".", "--auto-approve", "--parallelism", "1"],
        &[
            ("CARINA_MOCK_DELETE_DELAY_MS_FOR", "test.resource.z_blocked"),
            ("CARINA_MOCK_DELETE_DELAY_MS", "60000"),
            ("CARINA_MOCK_DELETE_READY_PATH", ready.to_str().unwrap()),
        ],
    );

    wait_for_file(&ready, Duration::from_secs(10));
    wait_for_state(&fixture.state, Duration::from_secs(5), |state| {
        state
            .find_resource("mock", "test.resource", FIRST)
            .is_none()
    });

    child.kill_with_sigkill();

    let state = load_state(&fixture.state);
    assert!(
        state
            .find_resource("mock", "test.resource", FIRST)
            .is_none(),
        "the completed delete must be durable before the run ends"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", BLOCKED)
            .is_some(),
        "the provider operation that never returned must remain in state"
    );

    let retry = fixture.output(
        &[
            "destroy",
            ".",
            "--auto-approve",
            "--parallelism",
            "1",
            "--lock=false",
        ],
        &[("CARINA_MOCK_DELETE_FAIL_FOR", "test.resource.a_completed")],
    );
    let stdout = String::from_utf8_lossy(&retry.stdout);
    assert!(
        stdout.contains("Plan: 1 to destroy."),
        "the follow-up destroy must include only the unfinished resource:\n{stdout}"
    );
    assert!(
        retry.status.success(),
        "the follow-up destroy must not retry the durable deletion: {}",
        output_text(&retry)
    );
}

struct ProjectFixture {
    _temp: TempDir,
    project: std::path::PathBuf,
    state: std::path::PathBuf,
    provider_state: std::path::PathBuf,
}

impl ProjectFixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let project = temp.path().to_path_buf();
        let state = project.join("carina.state.json");
        let provider_state = project.join("mock-provider-state.json");
        fs::write(
            project.join("main.crn"),
            format!(
                "backend local {{ path = \"{}\" }}\n\
                 provider mock {{}}\n\
                 let {FIRST} = mock.test.resource {{ name = \"{FIRST}\" }}\n\
                 let {BLOCKED} = mock.test.resource {{ name = \"{BLOCKED}\" }}\n",
                state.display()
            ),
        )
        .unwrap();
        fs::write(
            project.join("carina-backend.lock"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&serde_json::json!({
                    "backend_type": "local",
                    "attributes": { "path": state.display().to_string() },
                }))
                .unwrap()
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            project,
            state,
            provider_state,
        }
    }

    fn command(&self, args: &[&str], envs: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .args(args)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.provider_state)
            .env_remove("CLICOLOR_FORCE");
        for (name, value) in envs {
            command.env(name, value);
        }
        command
    }

    fn spawn(&self, args: &[&str], envs: &[(&str, &str)]) -> KillOnDrop {
        let mut command = self.command(args, envs);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        KillOnDrop::new(command.spawn().expect("spawn carina child process"))
    }

    fn output(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.command(args, envs)
            .output()
            .expect("run carina command")
    }
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        // wall-clock-ok: hang guard for state/readiness written by a real child process.
        assert!(
            Instant::now() < deadline,
            "provider readiness handshake was not written: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_state(
    path: &Path,
    timeout: Duration,
    predicate: impl Fn(&carina_state::StateFile) -> bool,
) {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(content) = fs::read_to_string(path)
            && let Ok(state) = carina_state::check_and_migrate(&content)
            && predicate(state.state())
        {
            return;
        }
        // wall-clock-ok: hang guard for state/readiness written by a real child process.
        assert!(
            Instant::now() < deadline,
            "state condition was not met before timeout: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn load_state(path: &Path) -> carina_state::StateFile {
    carina_state::check_and_migrate(&fs::read_to_string(path).unwrap())
        .unwrap()
        .into_state()
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

struct KillOnDrop {
    child: Option<Child>,
}

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn kill_with_sigkill(&mut self) {
        let child = self.child.as_mut().expect("child must still be running");
        // SAFETY: the PID belongs to the live child owned by this guard.
        let result = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        assert_eq!(result, 0, "failed to send SIGKILL to child");
        let status = child.wait().expect("wait for killed child");
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
        self.child.take();
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
