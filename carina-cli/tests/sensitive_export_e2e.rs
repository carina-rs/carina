//! Process-level export sensitivity regressions for carina#3849.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run(project: &std::path::Path, env_value: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(project)
        .env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("CARINA_3849_EXPORT_VALUE", env_value)
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
    let output = run(project, "unused", &["init", "."]);
    assert!(
        output.status.success(),
        "init failed:\n{}",
        output_text(&output),
    );
}

#[test]
fn env_derived_export_writes_plaintext_and_replan_is_stable() {
    let fixture = tempfile::tempdir().expect("tempdir");
    fs::write(
        fixture.path().join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .expect("backend fixture");
    fs::write(
        fixture.path().join("exports.crn"),
        r#"exports {
  a: String = upper(env("CARINA_3849_EXPORT_VALUE"))
}
"#,
    )
    .expect("exports fixture");

    let init = run(fixture.path(), "plainExportValue", &["init", "."]);
    assert!(
        init.status.success(),
        "init failed:\n{}",
        output_text(&init)
    );

    let apply = run(
        fixture.path(),
        "plainExportValue",
        &["apply", ".", "--auto-approve", "--lock=false"],
    );
    assert!(
        apply.status.success(),
        "apply failed:\n{}",
        output_text(&apply),
    );

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("carina.state.json"))
            .expect("state must be written"),
    )
    .expect("valid state JSON");
    assert_eq!(
        state.pointer("/exports/a"),
        Some(&serde_json::json!("PLAINEXPORTVALUE")),
        "env provenance must not alter export persistence: {state:#}",
    );

    let replan = run(fixture.path(), "plainExportValue", &["plan", "."]);
    let replan_text = output_text(&replan);
    assert!(replan.status.success(), "replan failed:\n{replan_text}");
    assert!(
        replan_text.contains("No changes"),
        "persisted env export must compare equal on re-plan:\n{replan_text}",
    );
}

#[test]
fn validate_and_plan_reject_secret_export_shapes_without_plaintext() {
    let fixture = tempfile::tempdir().expect("tempdir");
    fs::write(
        fixture.path().join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .expect("backend fixture");
    fs::write(
        fixture.path().join("exports.crn"),
        r#"let s = secret("topsecretXYZ")

exports {
  direct: String = s
  builtin: String = upper(s)
  interpolated: String = "x-${s}"
  inline_interpolated = "x-${secret("topsecretXYZ")}"
  nested_list: list(String) = [s]
  nested_map: map(String) = { key = s }
}
"#,
    )
    .expect("exports fixture");

    for args in [["validate", "."], ["plan", "."]] {
        let output = run(fixture.path(), "unused", &args);
        let text = output_text(&output);
        assert!(
            !output.status.success(),
            "{args:?} unexpectedly passed:\n{text}"
        );
        for name in [
            "direct",
            "builtin",
            "interpolated",
            "inline_interpolated",
            "nested_list",
            "nested_map",
        ] {
            assert!(
                text.contains(&format!("export '{name}' cannot hold a secret value")),
                "{args:?} missing error for {name}:\n{text}",
            );
        }
        assert!(
            !text.contains("topsecretXYZ"),
            "{args:?} leaked secret plaintext:\n{text}",
        );
    }
}

#[test]
fn commands_reject_secrets_reached_through_resource_and_module_attributes() {
    struct Case<'a> {
        name: &'a str,
        export: &'a str,
        files: &'a [(&'a str, &'a str)],
    }

    let cases = [
        Case {
            name: "resource attribute",
            export: "c",
            files: &[
                (
                    "providers.crn",
                    "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
                ),
                (
                    "main.crn",
                    r#"let r = mock.test.resource {
  name = "alpha"
  comment = secret("hunter2SECRET")
}
"#,
                ),
                ("exports.crn", "exports { c: String = r.comment }\n"),
            ],
        },
        Case {
            name: "module attribute",
            export: "e",
            files: &[
                (
                    "providers.crn",
                    "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
                ),
                (
                    "main.crn",
                    r#"let m = use { source = "./modules/pass" }
let inst = m { pw = secret("hunter2SECRET") }
exports { e: String = inst.out }
"#,
                ),
                (
                    "modules/pass/main.crn",
                    "arguments { pw: String }\nattributes { out: String = pw }\n",
                ),
            ],
        },
        Case {
            name: "derived module attribute",
            export: "x",
            files: &[
                (
                    "providers.crn",
                    r#"backend local { path = "carina.state.json" }
provider mock {}
let r = mock.test.resource { name = "alpha" }
"#,
                ),
                (
                    "main.crn",
                    r#"let m = use { source = "./modules/upper" }
let inst = m { pw = "${r.identifier}${secret("hunter2SECRET")}" }
exports { x: String = inst.out }
"#,
                ),
                (
                    "modules/upper/main.crn",
                    "arguments { pw: String }\nattributes { out: String = upper(pw) }\n",
                ),
            ],
        },
    ];

    for case in cases {
        let fixture = tempfile::tempdir().expect("tempdir");
        for (relative, contents) in case.files {
            write_file(fixture.path(), relative, contents);
        }
        initialize(fixture.path());

        let commands: &[&[&str]] = &[
            &["validate", "."],
            &["plan", "."],
            &["apply", ".", "--auto-approve", "--lock=false"],
        ];
        for args in commands {
            let output = run(fixture.path(), "unused", args);
            let rendered = output_text(&output);
            assert!(
                !output.status.success(),
                "{} {args:?} unexpectedly passed:\n{rendered}",
                case.name,
            );
            assert!(
                rendered.contains(&format!(
                    "export '{}' cannot hold a secret value",
                    case.export
                )),
                "{} {args:?} missed the secret export:\n{rendered}",
                case.name,
            );
            assert!(
                !rendered.contains("hunter2SECRET"),
                "{} {args:?} leaked plaintext:\n{rendered}",
                case.name,
            );
        }
    }
}

#[test]
fn apply_re_resolves_exports_from_replaced_resource_state() {
    let fixture = tempfile::tempdir().expect("tempdir");
    write_file(
        fixture.path(),
        "backend.crn",
        "backend local { path = 'carina.state.json' }\nprovider mock {}\n",
    );
    write_file(
        fixture.path(),
        "main.crn",
        r#"let r = mock.test.resource {
  name = "alpha"
  comment = "v1"
}

exports {
  id: String = r.identifier
  upper_id: String = upper(r.identifier)
  comment: String = r.comment
}
"#,
    );
    initialize(fixture.path());

    let first_apply = run(
        fixture.path(),
        "unused",
        &["apply", ".", "--auto-approve", "--lock=false"],
    );
    assert!(
        first_apply.status.success(),
        "first apply failed:\n{}",
        output_text(&first_apply),
    );

    write_file(
        fixture.path(),
        "main.crn",
        r#"let r = mock.test.resource {
  name = "beta"
  comment = "v2"
}

exports {
  id: String = r.identifier
  upper_id: String = upper(r.identifier)
  comment: String = r.comment
}
"#,
    );
    let second_apply = run(
        fixture.path(),
        "unused",
        &["apply", ".", "--auto-approve", "--lock=false"],
    );
    assert!(
        second_apply.status.success(),
        "replacement apply failed:\n{}",
        output_text(&second_apply),
    );

    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("carina.state.json"))
            .expect("state must be written"),
    )
    .expect("valid state JSON");
    assert_eq!(
        state.pointer("/exports/id"),
        Some(&serde_json::json!("beta-id"))
    );
    assert_eq!(
        state.pointer("/exports/upper_id"),
        Some(&serde_json::json!("BETA-ID")),
        "a builtin over a read-only attribute must use the post-replace value",
    );
    assert_eq!(
        state.pointer("/exports/comment"),
        Some(&serde_json::json!("v2")),
    );
    assert_eq!(
        state.pointer("/resources/0/current/attributes/identifier"),
        Some(&serde_json::json!("beta-id")),
    );
}

#[test]
fn validate_accumulates_secret_and_unrelated_export_type_errors() {
    let fixture = tempfile::tempdir().expect("tempdir");
    write_file(
        fixture.path(),
        "main.crn",
        r#"provider mock {}
let r = mock.test.resource {
  name = "alpha"
  comment = secret("hunter2SECRET")
}

exports {
  c: String = r.comment
  n: Int = "notint"
}
"#,
    );

    let output = run(fixture.path(), "unused", &["validate", "."]);
    let rendered = output_text(&output);
    assert!(!output.status.success(), "validate unexpectedly passed");
    assert!(
        rendered.contains("export 'c' cannot hold a secret value"),
        "missing secret export error:\n{rendered}",
    );
    assert!(
        rendered.contains("export 'n':")
            && rendered.contains("expected Int")
            && rendered.contains("got string 'notint'"),
        "missing unrelated export type error:\n{rendered}",
    );
    assert!(
        !rendered.contains("hunter2SECRET"),
        "secret plaintext leaked:\n{rendered}",
    );
}
