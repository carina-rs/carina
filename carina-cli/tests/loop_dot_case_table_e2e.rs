//! Executed #4591 case table, including complete diagnostics and saved plan data.

#[path = "common/loop_dot_cases.rs"]
mod cases;

use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .unwrap()
}

fn diagnostics(root: &Path) -> (bool, Vec<String>) {
    let output = run(root, &["validate", ".", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    let errors = report["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| entry["message"].as_str().unwrap().to_owned())
        .collect();
    (output.status.success(), errors)
}

fn plan(root: &Path) -> serde_json::Value {
    let output = run(root, &["plan", "--json", "--refresh=false", "."]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(plan.as_object_mut().unwrap().remove("timestamp").is_some());
    plan
}

#[test]
fn loop_dot_case_table() {
    run_case_table(|_| true);
}

#[test]
fn loop_dot_review_round3_bare_function_condition() {
    run_case_table(|row| row.id == "2-25");
}

#[test]
fn loop_dot_review_round3_whole_group_function() {
    run_case_table(|row| row.id == "2-24");
}

#[test]
fn loop_dot_review_round1_function_contexts() {
    run_case_table(|row| row.id == "2-19");
}

#[test]
fn loop_dot_review_round1_unrelated_cli_scope() {
    run_case_table(|row| matches!(row.id, "2-20" | "2-21"));
}

#[test]
fn loop_dot_review_round2_deferred_function_contexts() {
    run_case_table(|row| row.id == "2-22" || row.positions.contains(&"deferred function body"));
}

#[test]
fn loop_dot_review_round2_unrelated_export_failure() {
    run_case_table(|row| row.id == "2-23");
}

#[test]
fn loop_dot_review_round2_module_arguments() {
    run_case_table(|row| matches!(row.id, "2-2" | "2-17"));
}

#[test]
fn loop_dot_review_round2_non_loop_module_access_matches_main() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let row = cases::rows()
        .into_iter()
        .find(|row| row.id == "2-23")
        .unwrap();
    cases::fixture(root, &row);
    cases::write(
        root,
        "main.crn",
        "let child = use { source = './child' }\nlet m = child { value = 'x' }\n",
    );
    assert!(run(root, &["init", "."]).status.success());
    for (reader, expected) in [
        ("(m).x", vec![]),
        (
            "(m).missing",
            vec!["unknown attribute 'missing' on 'm' in reference m.missing"],
        ),
    ] {
        cases::write(
            root,
            "other.crn",
            &format!("let other = mock.test.resource {{ name = 'other' comment = {reader} }}\n"),
        );
        let (valid, errors) = diagnostics(root);
        assert_eq!(valid, expected.is_empty(), "{reader}");
        assert_eq!(errors, expected, "{reader}");
    }
}

fn run_case_table(include: impl Fn(&cases::Row) -> bool) {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let rows = cases::rows();
    cases::fixture(root, &rows[0]);
    let init = run(root, &["init", "."]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut failures = Vec::new();
    for row in rows.into_iter().filter(include) {
        cases::fixture(root, &row);
        for &position in row.positions {
            cases::reader(root, position, row.reader);
            let mut dot = diagnostics(root);
            let label = format!("{}, {:?}, {position}, {}", row.id, row.kind, row.reader);
            match row.expected {
                cases::Outcome::Validates if !dot.0 => failures.push(format!("{label}: {dot:?}")),
                cases::Outcome::Validates => {}
                cases::Outcome::Error(expected) => {
                    if dot.0 || dot.1 != expected {
                        failures.push(format!("{label}: {dot:?}, expected {expected:?}"));
                    }
                }
                cases::Outcome::SurfaceErrors { cli, lsp } => {
                    assert_ne!(cli, lsp, "each surface preserves main's wording");
                    if dot.0 || dot.1 != cli {
                        failures.push(format!("{label}: {dot:?}, expected {cli:?}"));
                    }
                }
                cases::Outcome::SameAsSubscript(subscript) => {
                    cases::reader(root, position, subscript);
                    let mut bracket = diagnostics(root);
                    if row.id == "2-6a" {
                        for (observed, reader) in
                            [(&mut dot, row.reader), (&mut bracket, subscript)]
                        {
                            assert_eq!(
                                observed.1,
                                [format!(
                                    "deferred-for iterable `{reader}`: expected list iterable, got map"
                                )]
                            );
                            observed.1[0] = observed.1[0].replace(reader, "<reader>");
                        }
                    }
                    if row.id == "2-22" && position == "deferred function body" {
                        assert_eq!(
                            bracket.1,
                            ["missing dependency edge to children.a provenance in ./other.crn"; 2],
                            "#4597 remains the subscript control"
                        );
                    }
                    if dot != bracket || (!matches!(row.id, "2-4" | "2-6a" | "2-22") && !dot.0) {
                        failures.push(format!("{label}: dot {dot:?}, subscript {bracket:?}"));
                    } else if row.id == "2-6a" && dot.0 {
                        failures.push(format!("{label}: simple map iteration must be rejected"));
                    } else if dot.0 {
                        let bracket_plan = plan(root);
                        cases::reader(root, position, row.reader);
                        let dot_plan = plan(root);
                        if dot_plan != bracket_plan {
                            failures.push(format!("{label}: dot/subscript plans differ:\n{dot_plan:#}\n{bracket_plan:#}"));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loop_dot_underscore_remains_a_syntax_error() {
    let root = tempfile::tempdir().unwrap();
    for kind in [
        cases::Kind::Map,
        cases::Kind::ValueKeyed,
        cases::Kind::Indexed,
    ] {
        let row = cases::Row {
            id: "2-16",
            kind,
            key: "_k",
            reader: "children._k.name",
            positions: cases::POSITIONS,
            expected: cases::Outcome::Error(&[]),
        };
        cases::fixture(root.path(), &row);
        for &position in row.positions {
            cases::reader(root.path(), position, row.reader);
            let (valid, errors) = diagnostics(root.path());
            assert!(
                !valid && errors.iter().any(|error| error.contains("Syntax error")),
                "{kind:?}, {position}: {errors:?}"
            );
        }
    }
}
