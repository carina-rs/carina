//! Executed #4591 case table, including complete diagnostics and saved plan data.

#[path = "common/loop_dot_cases.rs"]
mod cases;

#[path = "common/reported_loop_selector_cases.rs"]
mod reported_selector_cases;

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

// Measured on main 2ab61c021 with the same shared fixtures. Keep the complete
// error records so function-body/default locations cannot move to the selector.
fn assert_main_function_diagnostics(root: &Path, row: &cases::Row, position: &str) {
    let expected = cases::main_function_record(row, position);
    let label = format!("{}, {:?}, {position}, {}", row.id, row.kind, row.reader);
    let output = run(root, &["validate", ".", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(
        output.status.code().map(i64::from),
        expected["status"].as_i64(),
        "{label}"
    );
    assert_eq!(
        report
            .get("errors")
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        expected["errors"],
        "{label}"
    );
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
fn loop_dot_reported_selector_stops_expression_errors() {
    for module in [false, true] {
        assert_reported_selector_errors(reported_selector_cases::cases(), module);
    }
}

#[test]
fn loop_dot_reported_selector_after_another_failure_matches_current_diagnostics() {
    assert_reported_selector_errors([reported_selector_cases::input_i()], false);
    assert_reported_selector_errors([reported_selector_cases::input_i_cli_module()], true);
}

#[test]
fn loop_dot_reported_selector_stops_type_mismatch_in_same_attribute() {
    for case in [
        reported_selector_cases::Case {
            id: "selector-stops-type-mismatch",
            declaration: "let children = for (_, item) in ['a'] { mock.test.resource { name = item } }\n",
            attributes: "name = 'y' tags = { a = children.a.name, d = 1 }",
            errors: &["cannot apply access .a to list(mock.test.resource)"],
        },
        reported_selector_cases::Case {
            id: "fixed-selector-reveals-type-mismatch",
            declaration: "let children = for (_, item) in ['a'] { mock.test.resource { name = item } }\n",
            attributes: "name = 'y' tags = { a = children[0].name, d = 1 }",
            errors: &["expected String, got Int"],
        },
    ] {
        for module in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let root = fixture.path();
            let owner = reported_selector_cases::fixture(root, &case, module);
            cases::write(
                root,
                owner,
                &format!("mock.test.resource {{ {} }}\n", case.attributes),
            );
            assert!(run(root, &["init", "."]).status.success());
            let (success, errors) = diagnostics(root);
            assert!(!success, "{}, module={module}", case.id);
            assert_eq!(errors, case.errors, "{}, module={module}", case.id);
        }
    }
}

fn assert_reported_selector_errors(
    cases: impl IntoIterator<Item = reported_selector_cases::Case>,
    module: bool,
) {
    let mut failures = Vec::new();
    for case in cases {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        reported_selector_cases::fixture(root, &case, module);
        assert!(run(root, &["init", "."]).status.success());
        let (success, mut errors) = diagnostics(root);
        let mut expected = case.errors.to_vec();
        errors.sort();
        expected.sort();
        if success || errors != expected {
            failures.push(format!(
                "{}, module={module}: {errors:?}, expected exactly {expected:?}",
                case.id,
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loop_dot_case_table_module_internal_rejections_are_reported_once() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let mut failures = Vec::new();
    for row in cases::rows()
        .into_iter()
        .filter(|row| matches!(row.id, "2-3" | "2-11" | "2-13"))
    {
        cases::module_fixture(root, &row);
        assert!(run(root, &["init", "."]).status.success());
        let cases::Outcome::Error(expected) = row.expected else {
            unreachable!("rejected selector row");
        };
        for &position in &cases::POSITIONS[..7] {
            let owner = cases::module_reader(root, position, row.reader);
            let source = std::fs::read_to_string(root.join(&owner)).unwrap();
            let column = source.find(row.reader).unwrap() + 1;
            let output = run(root, &["validate", ".", "--json"]);
            let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let expected = serde_json::json!([{
                "message": expected[0],
                "severity": "error",
                "code": null,
                "location": {
                    "file": owner,
                    "range": {
                        "start": { "line": 1, "column": column },
                        "end": { "line": 1, "column": column + row.reader.len() },
                    },
                },
            }]);
            if output.status.code() != Some(1) || report["errors"] != expected {
                failures.push(format!(
                    "{}, {:?}, module-internal {position}, {}: {}, expected exactly {expected}",
                    row.id, row.kind, row.reader, report["errors"]
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn loop_dot_case_table_export_rejection_rendering() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let mut failures = Vec::new();
    for row in cases::rows()
        .into_iter()
        .filter(|row| matches!(row.id, "2-3" | "2-10" | "2-11" | "2-12" | "2-13"))
    {
        cases::fixture(root, &row);
        let source = std::fs::read_to_string(root.join("main.crn")).unwrap();
        cases::write(
            root,
            "main.crn",
            &source
                .replace("k, v in", "_, v in")
                .replace("(i, v) in", "(_, v) in"),
        );
        assert!(run(root, &["init", "."]).status.success());
        let cases::Outcome::Error([error]) = row.expected else {
            unreachable!("one rejected selector");
        };
        cases::reader(root, "export plain", row.reader);
        let expected_message =
            format!("./exports.crn:1:11: export 'out': type annotation required: {error}");
        let output = run(root, &["validate", ".", "--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let expected = serde_json::json!([{
            "message": expected_message,
            "severity": "error",
            "code": null,
            "location": null,
        }]);
        if report["errors"] != expected {
            failures.push(format!(
                "{}, {:?}, {}: JSON {} != {expected}",
                row.id, row.kind, row.reader, report["errors"]
            ));
        }
        let output = run(root, &["validate", "."]);
        let expected_text = format!("Validating...\nError: {expected_message}\n");
        let actual_text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if actual_text != expected_text {
            failures.push(format!(
                "{}, {:?}, {}: text {actual_text:?} != {expected_text:?}",
                row.id, row.kind, row.reader
            ));
        }
    }
    // The established subscript rejection uses the same export inference path.
    let row = cases::rows()
        .into_iter()
        .find(|row| row.id == "2-11" && row.kind == cases::Kind::Map)
        .unwrap();
    cases::fixture(root, &row);
    let source = std::fs::read_to_string(root.join("main.crn")).unwrap();
    cases::write(root, "main.crn", &source.replace("k, v in", "_, v in"));
    cases::reader(root, "export plain", "children[0].name");
    let message = "./exports.crn:1:11: export 'out': type annotation required: cannot apply access [0] to keyed loop group";
    let output = run(root, &["validate", ".", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["errors"],
        serde_json::json!([{
            "message": message,
            "severity": "error",
            "code": null,
            "location": null,
        }])
    );
    let output = run(root, &["validate", "."]);
    assert_eq!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        format!("Validating...\nError: {message}\n")
    );
    // Other rejected export rows already share their producer with bracket
    // spelling. Compare the complete terminal records to prevent a shape split.
    for row in cases::rows()
        .into_iter()
        .filter(|row| matches!(row.id, "2-2" | "2-4" | "2-15"))
    {
        cases::fixture(root, &row);
        let subscript = match row.id {
            "2-2" => "children['zzz'].name",
            "2-4" => "children['a']",
            "2-15" => "children['a'].b.name",
            _ => unreachable!(),
        };
        let mut rendered = Vec::new();
        for reader in [row.reader, subscript] {
            cases::reader(root, "export plain", reader);
            let output = run(root, &["validate", "."]);
            assert_eq!(output.status.code(), Some(1), "{row:?}, {reader}");
            rendered.push((output.stdout, output.stderr));
        }
        assert_eq!(rendered[0], rendered[1], "{row:?}");
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
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
fn loop_dot_function_body_and_default_keep_main_call_site_error() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    cases::write(root, "providers.crn", "provider mock {}\n");
    cases::write(
        root,
        "backend.crn",
        "backend local { path = 'carina.state.json' }\n",
    );
    let main: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/loop_dot_function_main.json")).unwrap();
    for (label, case) in main["direct_calls"].as_object().unwrap() {
        cases::write(root, "main.crn", case["source"].as_str().unwrap());
        let output = run(root, &["validate", "."]);
        assert_eq!(
            output.status.code().map(i64::from),
            case["status"].as_i64(),
            "{label}"
        );
        assert_eq!(
            format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
            case["text"].as_str().unwrap(),
            "{label}"
        );
        let output = run(root, &["validate", ".", "--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["errors"], case["errors"], "{label}");
    }
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
            if matches!(row.expected, cases::Outcome::MainFunction) {
                assert_main_function_diagnostics(root, &row, position);
                continue;
            }
            let mut dot = diagnostics(root);
            let label = format!("{}, {:?}, {position}, {}", row.id, row.kind, row.reader);
            match row.expected {
                cases::Outcome::MainFunction => unreachable!("main records checked above"),
                cases::Outcome::Validates if !dot.0 => failures.push(format!("{label}: {dot:?}")),
                cases::Outcome::Validates => {}
                cases::Outcome::Error(expected) => {
                    let expected = expected.iter().map(|message| {
                        if cases::inferred_selector_export(&row, position) {
                            format!("./exports.crn:1:11: export 'out': type annotation required: {message}")
                        } else {
                            (*message).to_owned()
                        }
                    }).collect::<Vec<_>>();
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
                    if dot != bracket || (!matches!(row.id, "2-4" | "2-6a") && !dot.0) {
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
