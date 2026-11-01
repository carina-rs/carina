//! Directory configurations exercise the same prepare seam for plan and apply.
use serde_json::{Value, json};
use std::fs;
use std::process::{Command, Output};
use tempfile::TempDir;

struct Project {
    dir: TempDir,
}
impl Project {
    fn new(attribute: &str) -> Self {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("providers.crn"), "provider mock {}\n").unwrap();
        fs::write(
            dir.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        let project = Self { dir };
        project.write(attribute, "alias/live");
        let initialized = project.run(&["init"], &table("key-a"), false);
        assert!(initialized.status.success(), "{}", output(&initialized));
        project
    }
    fn write(&self, attribute: &str, raw: &str) {
        fs::write(self.dir.path().join("main.crn"), format!("let example = mock.test.reference {{\n  name = 'example'\n  {attribute} = '{raw}'\n}}\n")).unwrap();
    }
    fn run(&self, args: &[&str], table: &Value, echo: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(args)
            .current_dir(self.dir.path())
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.dir.path().join("mock-state.json"),
            )
            .env("CARINA_MOCK_REFERENCE_TABLE", table.to_string())
            .env("CARINA_MOCK_REFERENCE_ECHO_ALIAS", echo.to_string())
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }
    fn apply(&self, table: &Value, echo: bool) {
        let result = self.run(&["apply", "--auto-approve"], table, echo);
        assert!(result.status.success(), "{}", output(&result));
    }
}
fn output(result: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    )
}
fn table(target: &str) -> Value {
    json!({"alias/live":{"Ok":{"Resolved":target}},"key-a":{"Ok":{"Resolved":"key-a"}},"key-b":{"Ok":{"Resolved":"key-b"}}})
}

#[test]
fn reference_directory_clean_replan_and_apply_replan_keep_authored_payloads() {
    let project = Project::new("key");
    let creation = project.run(
        &["plan", "--out", "create.plan.json"],
        &table("key-a"),
        false,
    );
    assert!(creation.status.success(), "{}", output(&creation));
    let creation = fs::read_to_string(project.dir.path().join("create.plan.json")).unwrap();
    assert!(creation.contains("alias/live"));
    assert!(!creation.contains("key-a"));
    project.apply(&table("key-a"), false);
    let result = project.run(&["plan", "--detailed-exitcode"], &table("key-a"), false);
    assert_eq!(result.status.code(), Some(0), "{}", output(&result));
    // A normal apply re-plans through the same witness seam.
    project.apply(&table("key-a"), false);
    let state = fs::read_to_string(project.dir.path().join("carina.state.json")).unwrap();
    assert!(state.contains("key-a"));
    assert!(
        !state.contains("key-b"),
        "state retains the provider read model"
    );
}

#[test]
fn reference_directory_repoint_plans_update_and_create_only_replacement() {
    for attribute in ["key", "fixed_key"] {
        let project = Project::new(attribute);
        project.apply(&table("key-a"), false);
        let result = project.run(
            &["plan", "--detailed-exitcode", "--out", "repoint.plan.json"],
            &table("key-b"),
            false,
        );
        assert_eq!(result.status.code(), Some(2), "{}", output(&result));
        let plan = fs::read_to_string(project.dir.path().join("repoint.plan.json")).unwrap();
        assert!(
            plan.contains("alias/live"),
            "saved effects keep raw references: {plan}"
        );
        assert!(
            !plan.contains("key-b"),
            "comparison target must not enter saved effects: {plan}"
        );
        assert!(
            plan.contains(if attribute == "key" {
                "Update"
            } else {
                "Create"
            }),
            "{plan}\n{}",
            output(&result)
        );
    }
}

#[test]
fn saved_reference_update_renders_and_applies_without_a_resolution_witness() {
    let project = Project::new("key");
    project.apply(&table("key-a"), false);
    let planned = project.run(
        &["plan", "--out", "saved.plan.json"],
        &table("key-b"),
        false,
    );
    assert!(planned.status.success(), "{}", output(&planned));
    let mut denied = table("key-b");
    denied["alias/live"] = json!({"Err":{"AccessDenied":{
        "kind":"api_error", "message":"saved-plan display must not resolve references",
        "provider_name":"mock", "operation":"ResolveKey", "code":"AccessDenied"
    }}});
    // Echo the authored input during CRUD so only an accidental planning or
    // display lookup could consult the deliberately denied resolver table.
    let applied = project.run(
        &["apply", "--auto-approve", "saved.plan.json"],
        &denied,
        true,
    );
    assert!(applied.status.success(), "{}", output(&applied));
    assert!(
        output(&applied).contains("alias/live"),
        "{}",
        output(&applied)
    );
    let state = fs::read_to_string(project.dir.path().join("carina.state.json")).unwrap();
    assert!(
        state.contains("alias/live"),
        "saved execution sends the authored payload: {state}"
    );
}

#[test]
fn reference_directory_echoed_alias_stays_clean_after_repoint() {
    let project = Project::new("key");
    project.apply(&table("key-a"), true);
    let result = project.run(&["plan", "--detailed-exitcode"], &table("key-b"), true);
    assert_eq!(result.status.code(), Some(0), "{}", output(&result));
}

#[test]
fn reference_directory_dangling_current_is_a_diff_with_an_explanation() {
    let project = Project::new("key");
    project.apply(&table("key-a"), false);
    let mut table = table("key-b");
    table.as_object_mut().unwrap().remove("key-a");
    let result = project.run(&["plan", "--detailed-exitcode"], &table, false);
    assert_eq!(result.status.code(), Some(2), "{}", output(&result));
    assert!(
        output(&result).contains("current reference no longer resolves"),
        "{}",
        output(&result)
    );
}

#[test]
fn reference_directory_denial_exits_one_and_produces_no_executable_saved_plan() {
    let project = Project::new("key");
    project.apply(&table("key-a"), false);
    let mut table = table("key-a");
    table["alias/live"] = json!({"Err":{"AccessDenied":{
        "kind":"api_error", "message":"Describe denied", "provider_name":"mock",
        "operation":"ResolveKey", "code":"AccessDenied", "request_id":"request-123"
    }}});
    let result = project.run(&["plan", "--out", "denied.plan.json"], &table, false);
    assert_eq!(result.status.code(), Some(1), "{}", output(&result));
    for part in [
        "example",
        "key",
        "alias/live",
        "AccessDenied",
        "request-123",
    ] {
        assert!(
            output(&result).contains(part),
            "missing {part}: {}",
            output(&result)
        );
    }
    assert_eq!(
        output(&result).matches("request-123").count(),
        1,
        "{}",
        output(&result)
    );
    assert!(!project.dir.path().join("denied.plan.json").exists());
}
