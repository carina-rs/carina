#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

#[test]
fn sigint_then_sigterm_flushes_completed_state_releases_lock_and_exits_130() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path();
    let state_path = project.join("carina.state.json");
    let lock_path = state_path.with_extension("lock");
    let provider_state_path = project.join("mock-provider-state.json");
    let provider_ready_path = project.join("blocked-create-ready");

    fs::write(
        project.join("main.crn"),
        format!(
            "backend local {{ path = \"{}\" }}\n\
             provider mock {{}}\n\
             let a_completed = mock.test.resource {{ name = \"a_completed\" }}\n\
             let z_blocked = mock.test.resource {{ name = \"z_blocked\" }}\n",
            state_path.display()
        ),
    )
    .unwrap();
    fs::write(
        project.join("carina-backend.lock"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "backend_type": "local",
                "attributes": { "path": state_path.display().to_string() },
            }))
            .unwrap()
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .args(["apply", ".", "--auto-approve", "--parallelism", "1"])
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_STATE_FILE", &provider_state_path)
        .env("CARINA_MOCK_CREATE_DELAY_MS_FOR", "test.resource.z_blocked")
        .env("CARINA_MOCK_CREATE_DELAY_MS", "60000")
        .env("CARINA_MOCK_CREATE_READY_PATH", &provider_ready_path)
        .env_remove("CLICOLOR_FORCE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn carina apply");

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || read_to_string(stdout));
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut captured = String::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.expect("read child stderr");
            captured.push_str(&line);
            captured.push('\n');
            let _ = stderr_tx.send(line);
        }
        captured
    });
    let mut child = KillOnDrop::new(child);

    wait_for_file(&provider_ready_path, Duration::from_secs(10));
    send_signal(child.id(), libc::SIGINT);
    wait_for_stderr(
        &stderr_rx,
        "Received shutdown signal: Interrupt.",
        Duration::from_secs(5),
    );
    send_signal(child.id(), libc::SIGTERM);

    let status = child
        .wait_with_timeout(Duration::from_secs(5))
        .expect("carina must exit after cancellation cleanup");
    let stdout = stdout_reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();

    assert_eq!(
        status.code(),
        Some(130),
        "unexpected exit status\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !lock_path.exists(),
        "the real SIGINT/SIGTERM path must remove {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        lock_path.display()
    );

    let state = carina_state::check_and_migrate(&fs::read_to_string(&state_path).unwrap())
        .unwrap()
        .into_state();
    assert!(
        state
            .find_resource("mock", "test.resource", "a_completed")
            .is_some(),
        "the provider result completed before SIGINT must be persisted"
    );
    assert!(
        state
            .find_resource("mock", "test.resource", "z_blocked")
            .is_none(),
        "the in-flight provider operation abandoned after SIGTERM must not be persisted"
    );
    assert!(stderr.contains("Received shutdown signal: Terminate."));
    assert!(stderr.contains("Cancellation cleanup: state flushed."));
    assert!(stderr.contains("Cancellation cleanup: state lock released."));
}

#[test]
fn abandoned_destroy_reports_unknown_outcome_separately_from_skipped() {
    let fixture = TempDir::new().unwrap();
    let project = fixture.path();
    let state_path = project.join("carina.state.json");
    let provider_state_path = project.join("mock-provider-state.json");
    let provider_ready_path = project.join("blocked-delete-ready");

    fs::write(
        project.join("main.crn"),
        format!(
            "backend local {{ path = \"{}\" }}\n\
             provider mock {{}}\n\
             let a_completed = mock.test.resource {{ name = \"a_completed\" }}\n\
             let z_blocked = mock.test.resource {{ name = \"z_blocked\" }}\n",
            state_path.display()
        ),
    )
    .unwrap();
    fs::write(
        project.join("carina-backend.lock"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&serde_json::json!({
                "backend_type": "local",
                "attributes": { "path": state_path.display().to_string() },
            }))
            .unwrap()
        ),
    )
    .unwrap();

    let setup = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .args(["apply", ".", "--auto-approve", "--parallelism", "1"])
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_STATE_FILE", &provider_state_path)
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("apply destroy fixture");
    assert!(
        setup.status.success(),
        "fixture apply failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&setup.stdout),
        String::from_utf8_lossy(&setup.stderr),
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .args([
            "destroy",
            ".",
            "--auto-approve",
            "--refresh=false",
            "--parallelism",
            "1",
        ])
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_STATE_FILE", &provider_state_path)
        .env("CARINA_MOCK_DELETE_DELAY_MS_FOR", "test.resource.z_blocked")
        .env("CARINA_MOCK_DELETE_DELAY_MS", "60000")
        .env("CARINA_MOCK_DELETE_READY_PATH", &provider_ready_path)
        .env_remove("CLICOLOR_FORCE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn carina destroy");

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let stdout_reader = thread::spawn(move || read_to_string(stdout));
    let (stderr_tx, stderr_rx) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut captured = String::new();
        for line in BufReader::new(stderr).lines() {
            let line = line.expect("read child stderr");
            captured.push_str(&line);
            captured.push('\n');
            let _ = stderr_tx.send(line);
        }
        captured
    });
    let mut child = KillOnDrop::new(child);

    wait_for_file(&provider_ready_path, Duration::from_secs(10));
    send_signal(child.id(), libc::SIGINT);
    wait_for_stderr(
        &stderr_rx,
        "Received shutdown signal: Interrupt.",
        Duration::from_secs(5),
    );
    send_signal(child.id(), libc::SIGTERM);

    let status = child
        .wait_with_timeout(Duration::from_secs(5))
        .expect("carina destroy must exit after cancellation cleanup");
    let stdout = stdout_reader.join().unwrap();
    let stderr = stderr_reader.join().unwrap();

    assert_eq!(
        status.code(),
        Some(130),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Delete mock.test.resource z_blocked - interrupted (outcome unknown)"),
        "abandoned delete must say its outcome is unknown\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stdout
            .contains("Destroy interrupted. 1 succeeded, 0 failed, 0 skipped, 1 outcome unknown."),
        "unknown outcomes must be counted separately from skipped resources\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

fn wait_for_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        // wall-clock-ok: hang guard for a real child-process readiness file.
        assert!(
            Instant::now() < deadline,
            "provider readiness handshake was not written: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_stderr(lines: &Receiver<String>, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // wall-clock-ok: hang guard for stderr from a real child process.
        let line = lines
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("child stderr never contained {needle:?}"));
        if line.contains(needle) {
            return;
        }
    }
}

fn send_signal(pid: u32, signal: libc::c_int) {
    // SAFETY: `pid` belongs to the live child owned by KillOnDrop and `signal`
    // is one of libc's platform signal constants.
    let result = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(result, 0, "failed to send signal {signal} to pid {pid}");
}

fn read_to_string(mut reader: impl Read) -> String {
    let mut captured = String::new();
    reader.read_to_string(&mut captured).unwrap();
    captured
}

struct KillOnDrop {
    child: Option<Child>,
}

impl KillOnDrop {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn wait_with_timeout(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                self.child.take();
                return Some(status);
            }
            // wall-clock-ok: hang guard: terminate a stuck child process, not a speed assertion.
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(10));
        }
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
