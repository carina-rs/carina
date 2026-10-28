use std::path::Path;
use std::process::{Command, Output};

fn carina_with_nextest_home(home: &Path, test_home: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .args(["skills", "install"])
        .env("HOME", home)
        .env("NEXTEST_RUN_ID", "user-dirs-guard-e2e")
        .env("CARINA_TRUST_RECORD_DIR", home.join(".carina/trust"))
        .env("CARINA_PLUGIN_CACHE_DIR", home.join(".carina/plugin-cache"));
    match test_home {
        Some(test_home) => {
            command.env("CARINA_TEST_HOME", test_home);
        }
        None => {
            command.env_remove("CARINA_TEST_HOME");
        }
    }
    command.output().expect("spawn carina")
}

#[test]
fn spawned_carina_rejects_a_nextest_home_mismatch() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("actual-home");
    let marker = temp.path().join("different-test-home");
    std::fs::create_dir_all(&home).unwrap();

    let output = carina_with_nextest_home(&home, Some(&marker));

    assert!(!output.status.success(), "guard bypassed: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("nextest home isolation guard"),
        "unexpected stderr: {stderr}"
    );
    assert!(!home.join(".agents/skills/carina/SKILL.md").exists());
}

#[test]
fn spawned_carina_uses_the_nextest_setup_home_when_the_marker_matches() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = carina_with_nextest_home(&home, Some(&home));

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let installed = home.join(".agents/skills/carina/SKILL.md");
    assert!(
        installed.is_file(),
        "{} was not written",
        installed.display()
    );
    assert!(installed.starts_with(&home));
}
