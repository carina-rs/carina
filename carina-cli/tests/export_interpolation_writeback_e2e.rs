//! Process-level regressions for exports that interpolate resource attributes (carina#3860).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_MOCK_STATE_FILE")
        .args(args)
        .output()
        .expect("run carina")
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn initialize(project: &Path) {
    let output = run(project, &["init", "."]);
    assert!(
        output.status.success(),
        "init failed:\n{}",
        output_text(&output),
    );
}

enum Layout {
    MultiFile,
    SingleFile,
}

fn assert_export_interpolation_writeback(layout: Layout) {
    const RESOURCE: &str = r#"let r = mock.test.resource {
  name = "alpha"
  comment = "c"
}
"#;
    const EXPORTS: &str = r#"exports {
  interpolated_name: String = "${r.name}"
  prefixed_comment: String = "x-${r.comment}"
  interpolated_identifier: String = "id-${r.identifier}"
  plain_name: String = r.name
}
"#;

    let fixture = tempfile::tempdir().expect("tempdir");
    write_file(
        fixture.path(),
        "backend.crn",
        "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
    );
    match layout {
        Layout::MultiFile => {
            write_file(fixture.path(), "main.crn", RESOURCE);
            write_file(fixture.path(), "exports.crn", EXPORTS);
        }
        Layout::SingleFile => {
            write_file(
                fixture.path(),
                "main.crn",
                &format!("{RESOURCE}\n{EXPORTS}"),
            );
        }
    }
    initialize(fixture.path());

    let plan = run(fixture.path(), &["plan", "."]);
    let plan_text = output_text(&plan);
    assert!(plan.status.success(), "plan failed:\n{plan_text}");
    assert!(
        plan_text.contains("interpolated_name: String = 'alpha'"),
        "plan omitted the resolved interpolated name:\n{plan_text}",
    );
    assert!(
        plan_text.contains("prefixed_comment: String = 'x-c'"),
        "plan omitted the resolved prefixed comment:\n{plan_text}",
    );
    assert!(
        !plan_text.contains("${"),
        "plan retained raw interpolation:\n{plan_text}",
    );

    let apply = run(
        fixture.path(),
        &["apply", ".", "--auto-approve", "--lock=false"],
    );
    let apply_text = output_text(&apply);
    assert!(apply.status.success(), "apply failed:\n{apply_text}");
    assert!(
        !apply_text.contains("not written"),
        "apply skipped an export:\n{apply_text}",
    );

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("carina.state.json"))
            .expect("state must be written"),
    )
    .expect("valid state JSON");
    assert_eq!(
        state["exports"],
        serde_json::json!({
            "interpolated_name": "alpha",
            "prefixed_comment": "x-c",
            "interpolated_identifier": "id-alpha-id",
            "plain_name": "alpha",
        }),
    );

    let replan = run(fixture.path(), &["plan", "."]);
    let replan_text = output_text(&replan);
    assert!(replan.status.success(), "replan failed:\n{replan_text}");
    assert!(
        replan_text.contains("No changes"),
        "persisted exports must compare equal on re-plan:\n{replan_text}",
    );
}

#[test]
fn multi_file_layout_writes_interpolated_exports() {
    assert_export_interpolation_writeback(Layout::MultiFile);
}

#[test]
fn single_file_layout_writes_interpolated_exports() {
    assert_export_interpolation_writeback(Layout::SingleFile);
}
