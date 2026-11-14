use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const CHILD: &str = "CARINA_LSP_REPLAY_CHILD";
pub const MANIFEST: &str = "CARINA_LSP_REPLAY_MANIFEST";
pub const WORKER_OUTPUT: &str = "CARINA_LSP_REPLAY_WORKER_OUTPUT";

#[derive(Deserialize)]
enum CompletedStage {
    #[serde(rename = "complete")]
    Complete,
}

#[derive(Deserialize)]
struct CompletedRecord {
    stage: CompletedStage,
}

pub struct CompletedWorker {
    output: PathBuf,
    elapsed: Duration,
}

impl CompletedWorker {
    pub fn output(&self) -> &Path {
        &self.output
    }

    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

pub fn worker(
    test: &str,
    manifest: &Path,
    output: &Path,
    home: &Path,
    timeout: Duration,
    ignored: bool,
) -> CompletedWorker {
    std::fs::create_dir(output).expect("replay worker output must be fresh");
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
    let record = std::fs::read(output.join("stage.json")).unwrap_or_else(|error| {
        panic!(
            "LSP replay worker did not complete: {error}; evidence: {}",
            output.display()
        )
    });
    let CompletedRecord {
        stage: CompletedStage::Complete,
    } = serde_json::from_slice(&record).unwrap_or_else(|error| {
        panic!(
            "LSP replay worker did not complete: {error}; evidence: {}",
            output.display()
        )
    });
    CompletedWorker {
        output: output.to_path_buf(),
        elapsed: start.elapsed(),
    }
}
