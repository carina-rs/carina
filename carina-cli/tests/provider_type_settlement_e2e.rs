use std::process::{Command, Output};

const FAILED: &str = "provider broken { source = 'file:///definitely-missing/broken.wasm' }\n";
const ATTRIBUTE: &str = "attributes {\n  x: AwsAccountId = '123'\n}\n";
const FUNCTIONS: &str = "fn f(x: Arnn) { x }\nfn g(): Bogus { 'x' }\n";
const ATTRIBUTE_D: &str = "attributes {\n  y: AwsAccountId = '1'\n}\n";

fn run(command: &str, source: &str) -> Output {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.crn"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args([command, directory.path().to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn settlement_fixture_a_failed_provider_defers_attribute_once() {
    let output = run("validate", &format!("{FAILED}{ATTRIBUTE}"));
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(
        text.matches("was deferred because provider schemas were not loaded")
            .count(),
        1,
        "{text}"
    );
    assert!(!text.contains("unknown custom type"), "{text}");
    assert!(text.contains("main.crn:3:3:"), "{text}");
}

#[test]
fn settlement_fixture_b_failed_provider_defers_uncalled_annotations() {
    let output = run("validate", &format!("{FAILED}{FUNCTIONS}"));
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(
        text.matches("was deferred because provider schemas were not loaded")
            .count(),
        2,
        "{text}"
    );
    assert!(!text.contains("unknown custom type"), "{text}");
    assert!(text.contains("Arnn") && text.contains("Bogus"), "{text}");
    assert!(
        text.contains("main.crn:2:9:") && text.contains("main.crn:3:9:"),
        "{text}"
    );
}

#[test]
fn settlement_fixture_c_empty_loaded_catalog_rejects_attribute() {
    let output = run("validate", ATTRIBUTE);
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(
        text.matches("unknown custom type 'AwsAccountId'").count(),
        1,
        "{text}"
    );
    assert!(!text.contains("was deferred"), "{text}");
}

#[test]
fn settlement_fixture_d_empty_loaded_catalog_rejects_all_annotations() {
    let output = run("validate", &format!("{FUNCTIONS}{ATTRIBUTE_D}"));
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    for name in ["Arnn", "Bogus", "AwsAccountId"] {
        assert_eq!(
            text.matches(&format!("unknown custom type '{name}'"))
                .count(),
            1,
            "{text}"
        );
    }
    assert!(!text.contains("was deferred"), "{text}");
}

#[test]
fn settlement_lint_failed_provider_is_a_finding_without_annotations() {
    let output = run("lint", FAILED);
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(!text.contains("No lint warnings found"), "{text}");
    assert!(
        text.contains("broken") && text.contains("Found 1 lint warning(s)"),
        "{text}"
    );
}

#[test]
fn settlement_lint_failed_provider_reports_all_uncalled_annotations() {
    let output = run("lint", &format!("{FAILED}{FUNCTIONS}{ATTRIBUTE}"));
    let text = text(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(
        text.matches("was deferred because provider schemas were not loaded")
            .count(),
        3,
        "{text}"
    );
    assert!(!text.contains("unknown custom type"), "{text}");
}

#[test]
fn settlement_factoryless_provider_is_non_authoritative() {
    let source = format!("provider missing {{}}\n{FUNCTIONS}{ATTRIBUTE}");
    for command in ["validate", "lint"] {
        let output = run(command, &source);
        let text = text(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(
            text.matches("was deferred because provider schemas were not loaded")
                .count(),
            3,
            "{text}"
        );
        assert!(!text.contains("unknown custom type"), "{text}");
    }
}
