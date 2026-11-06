use std::{
    fs,
    process::{Command, Output},
};
use tempfile::TempDir;

struct Fixture(TempDir);

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("providers.crn"), "provider mock {}\n").unwrap();
        fs::write(
            dir.path().join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("values.crn"),
            "let legacy_name = \"192.168.1.1\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "let legacy = mock.test.creation_constrained { name = legacy_name }\n",
        )
        .unwrap();
        let fixture = Self(dir);
        assert!(fixture.run(&["init", "."]).status.success());
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.0.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.0.path().join("mock-state.json"),
            )
            .env("CARINA_MOCK_OP_LOG", self.0.path().join("op.log"))
            .args(args)
            .output()
            .unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn refined_multifile_template_is_proven_during_plan() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("values.crn"),
        "let producer = mock.test.estimated_input {}\nlet var = producer.short_part\n",
    )
    .unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let consumer = mock.test.estimated_input { name = \"${var}-suffix\" }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        !text(&output).contains("input check pending"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(fixture.0.path().join("plan.json").exists());
}

#[test]
fn refined_multifile_template_violation_is_a_value_free_plan_error() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("values.crn"),
        "let producer = mock.test.estimated_input {}\nlet var = producer.long_part\n",
    )
    .unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let consumer = mock.test.estimated_input { name = \"${var}-suffix\" }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(!text(&output).contains("-suffix"), "{}", text(&output));
    assert!(!fixture.0.path().join("plan.json").exists());
    assert!(!fixture.0.path().join("op.log").exists());
}

#[test]
fn creation_rejection_exits_one_and_never_creates_or_overwrites_plan() {
    let fixture = Fixture::new();
    for existing in [false, true] {
        let plan = fixture.0.path().join("plan.json");
        if existing {
            fs::write(&plan, "keep existing plan").unwrap();
        }
        for detailed in [false, true] {
            let mut args = vec!["plan", "--refresh=false", "--out", "plan.json"];
            if detailed {
                args.push("--detailed-exitcode");
            }
            let output = fixture.run(&args);
            assert_eq!(output.status.code(), Some(1), "{}", text(&output));
            assert!(
                text(&output).contains("mock.name.not_ipv4"),
                "{}",
                text(&output)
            );
            assert!(text(&output).contains("cannot create"), "{}", text(&output));
            assert!(!text(&output).contains("192.168.1.1"), "{}", text(&output));
            if existing {
                assert_eq!(fs::read_to_string(&plan).unwrap(), "keep existing plan");
            } else {
                assert!(!plan.exists());
            }
        }
    }
}

#[test]
fn creation_rejection_precedes_json_and_live_apply_effects() {
    let fixture = Fixture::new();
    for args in [
        vec!["plan", "--refresh=false", "--json"],
        vec!["apply", "--auto-approve", "--lock=false"],
    ] {
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert!(
            text(&output).contains("mock.name.not_ipv4"),
            "{}",
            text(&output)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("\"version\""));
        assert!(!stdout.contains("\"plan\""));
        assert!(!fixture.0.path().join("op.log").exists());
    }
}

#[test]
fn import_of_legacy_name_is_suppressed_before_creation_admission() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("imports.crn"),
        "import { to = mock.test.creation_constrained \"legacy\" id = \"existing-id\" }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    let plan = fs::read_to_string(fixture.0.path().join("plan.json")).unwrap();
    assert!(plan.contains("\"Import\""));
    assert!(!plan.contains("\"Create\""));
}

#[test]
fn validate_has_no_creation_findings_for_legacy_name() {
    let fixture = Fixture::new();
    let output = fixture.run(&["validate", "."]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!text(&output).contains("mock.name.not_ipv4"));
}

#[test]
fn pending_inputs_saves_without_certificates_and_live_apply_stops_before_all_effects() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("main.crn"), "let suffix = mock.test.resource { name = 'suffix' }\nlet pending = mock.test.creation_constrained { name = suffix.identifier }\n").unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("input check pending until apply"),
        "{}",
        text(&output)
    );
    let saved = fs::read_to_string(fixture.0.path().join("plan.json")).unwrap();
    for field in [
        "pending_inputs",
        "PendingInputObligation",
        "InputCheckedPlan",
        "paired_delete",
    ] {
        assert!(!saved.contains(field));
    }
    let output = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("input check pending; apply-time discharge not implemented"),
        "{}",
        text(&output)
    );
    assert!(!fixture.0.path().join("op.log").exists());
    assert!(!fixture.0.path().join("carina.state.json").exists());
}

#[test]
fn replacement_rejection_keeps_old_resource_and_runs_no_mutations() {
    for (resource_type, cbd) in [
        ("test.creation_constrained", false),
        ("test.creation_constrained", true),
        ("test.creation_conflicting", false),
    ] {
        let fixture = Fixture::new();
        fs::write(
            fixture.0.path().join("main.crn"),
            format!("let legacy = mock.{resource_type} {{ name = 'valid-name' generation = 1 }}\n"),
        )
        .unwrap();
        let seed = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
        assert!(seed.status.success(), "{}", text(&seed));
        let before = fs::read(fixture.0.path().join("carina.state.json")).unwrap();
        let provider_before = fs::read(fixture.0.path().join("mock-state.json")).unwrap();
        fs::write(fixture.0.path().join("op.log"), "").unwrap();
        fs::write(fixture.0.path().join("main.crn"), format!("let legacy = mock.{resource_type} {{ name = legacy_name generation = 2 directives {{ create_before_destroy = {cbd} }} }}\n")).unwrap();
        let output = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
        assert_eq!(output.status.code(), Some(1), "{}", text(&output));
        assert!(
            text(&output).contains("mock.name.not_ipv4"),
            "{}",
            text(&output)
        );
        let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
        assert!(
            operations.lines().all(|line| line.starts_with("read ")),
            "{operations}"
        );
        assert_eq!(
            fs::read(fixture.0.path().join("carina.state.json")).unwrap(),
            before
        );
        assert_eq!(
            fs::read(fixture.0.path().join("mock-state.json")).unwrap(),
            provider_before
        );
    }
}

#[test]
fn saved_plan_without_input_rules_still_applies() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let plain = mock.test.resource { name = 'plain' }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    let output = fixture.run(&["apply", "--auto-approve", "--lock=false", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
}

#[test]
fn input_default_tags_reserved_prefix_is_rejected_before_plan_output() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("providers.crn"),
        "provider mock { default_tags = { 'AWS:private-key' = 'secret-default-value' } }\n",
    )
    .unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let tagged = mock.test.input_constrained { name = 'valid-name' }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.tags.reserved_prefix"),
        "{}",
        text(&output)
    );
    assert!(!text(&output).contains("AWS:private-key"));
    assert!(!text(&output).contains("secret-default-value"));
    assert!(!fixture.0.path().join("plan.json").exists());
}

#[test]
fn input_validate_reports_every_write_but_not_create_only_in_multi_file_directory() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("values.crn"),
        "let legacy_name = '192.168.1.1'\nlet tags = { 'aWs:private-key' = 'secret-tag-value' }\n",
    )
    .unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let tagged = mock.test.input_constrained { name = legacy_name tags = tags }\n",
    )
    .unwrap();
    let output = fixture.run(&["validate", "."]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.tags.reserved_prefix"),
        "{}",
        text(&output)
    );
    assert!(!text(&output).contains("mock.name.not_ipv4"));
    assert!(!text(&output).contains("aWs:private-key"));
    assert!(!text(&output).contains("secret-tag-value"));
}

#[test]
fn input_update_rejects_authored_reserved_tag_without_mutating_state() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let tagged = mock.test.input_constrained { name = 'valid-name' comment = 'before' }\n",
    )
    .unwrap();
    let seed = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
    assert!(seed.status.success(), "{}", text(&seed));
    let before = fs::read(fixture.0.path().join("carina.state.json")).unwrap();
    fs::write(fixture.0.path().join("main.crn"),
        "let tagged = mock.test.input_constrained { name = 'valid-name' comment = 'after' tags = { 'aws:probe' = 'secret' } }\n").unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.tags.reserved_prefix"),
        "{}",
        text(&output)
    );
    assert_eq!(
        fs::read(fixture.0.path().join("carina.state.json")).unwrap(),
        before
    );
}

#[test]
fn input_rules_do_not_reject_import_desired_values_that_are_never_written() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("main.crn"),
        "let tagged = mock.test.input_constrained { name = 'valid-name' tags = { 'aws:cloudformation:stack-id' = 'observed' } }\n").unwrap();
    fs::write(
        fixture.0.path().join("imports.crn"),
        "import { to = mock.test.input_constrained 'tagged' id = 'existing-id' }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!text(&output).contains("mock.tags.reserved_prefix"));
    let saved = fs::read_to_string(fixture.0.path().join("plan.json")).unwrap();
    assert!(saved.contains("\"Import\""));
    assert!(!saved.contains("\"Create\""));
}
