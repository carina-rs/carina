use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn cli(project: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .args(arguments)
        .output()
        .expect("run carina")
}

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let upstream = root.path().join("missing");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&upstream).unwrap();
    fs::write(
        upstream.join("main.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
    fs::write(project.join("main.crn"), "backend local { path = 'carina.state.json' }\nlet mgmt = upstream_state { source = '../missing' }\nmock.test.resource { name = 'tagged' }\n").unwrap();
    let init = cli(&project, &["init", "."]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    fs::write(
        project.join("providers.crn"),
        "provider mock { default_tags = mgmt.tags }\n",
    )
    .unwrap();
    (root, project)
}

#[test]
fn issue_4304_3a_review5_plain_plan_whole_map_missing_upstream() {
    let (_root, project) = fixture();
    let output = cli(&project, &["plan", "."]);
    println!(
        "plain plan exit: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !output.status.success(),
        "whole-map missing-upstream characterization: plain plan displayed the plan"
    );
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    for part in ["mock", "default_tags", "mgmt"] {
        assert!(diagnostic.contains(part), "{diagnostic}");
    }
}

#[test]
fn issue_4304_3a_review5_strict_whole_map_missing_upstream_names_the_slot() {
    let (_root, project) = fixture();
    for arguments in [
        vec!["plan", "--out", "plan.json", "."],
        vec!["plan", "--json", "."],
        vec!["apply", ".", "--auto-approve"],
    ] {
        let output = cli(&project, &arguments);
        println!(
            "{arguments:?}: {:?}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.status.success(),
            "{arguments:?} accepted unknown defaults"
        );
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        for part in ["mock", "default_tags", "mgmt"] {
            assert!(diagnostic.contains(part), "{arguments:?}: {diagnostic}");
        }
        assert!(!project.join("plan.json").exists());
    }
}
