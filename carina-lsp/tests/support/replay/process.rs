use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CHILD: &str = "CARINA_LSP_REPLAY_CHILD";
pub const MANIFEST: &str = "CARINA_LSP_REPLAY_MANIFEST";
pub const WORKER_OUTPUT: &str = "CARINA_LSP_REPLAY_WORKER_OUTPUT";

pub fn worker(
    test: &str,
    manifest: &Path,
    output: &Path,
    home: &Path,
    timeout: Duration,
    ignored: bool,
) -> Duration {
    std::fs::create_dir_all(output).unwrap();
    let stdout = std::fs::File::create(output.join("stdout.log")).unwrap();
    let stderr = std::fs::File::create(output.join("stderr.log")).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--nocapture"]);
    if ignored {
        command.arg("--ignored");
    }
    let mut child = command
        .env(CHILD, "1")
        .env(MANIFEST, manifest)
        .env(WORKER_OUTPUT, output)
        .env("HOME", home)
        .env("TMPDIR", home.join("tmp"))
        .env("CARINA_TEST_HOME", home)
        .env("CARINA_TRUST_RECORD_DIR", home.join(".carina/trust"))
        .env("CARINA_PLUGIN_CACHE_DIR", home.join(".carina/plugin-cache"))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let start = Instant::now();
    let deadline = start + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        // wall-clock-ok: hang guard: kill and reap a stuck replay worker.
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "LSP replay worker timed out; evidence: {}",
                output.display()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        status.success(),
        "LSP replay worker failed ({status}); evidence: {}",
        output.display()
    );
    start.elapsed()
}
