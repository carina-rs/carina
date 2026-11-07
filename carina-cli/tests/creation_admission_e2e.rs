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
fn live_input_discharge_multifile_runtime_violation_preserves_replaced_resource() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("values.crn"), "").unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let legacy = mock.test.estimated_input { name = 'valid-name' generation = 1 }\n",
    )
    .unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
    assert!(output.status.success(), "{}", text(&output));
    let before: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    fs::write(fixture.0.path().join("op.log"), "").unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let producer = mock.test.resource { name = 'long-apply-only-identifier' }\n\
         let legacy = mock.test.estimated_input { name = producer.identifier generation = 2 }\n",
    )
    .unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "--lock=false"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("long-apply-only-identifier-id"),
        "{}",
        text(&output)
    );
    let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
    let creates: Vec<_> = operations
        .lines()
        .filter(|line| line.starts_with("create "))
        .collect();
    assert_eq!(
        creates.len(),
        1,
        "only the prerequisite may mutate: {operations}"
    );
    assert!(creates[0].contains("producer"), "{operations}");
    assert!(
        !operations.lines().any(|line| line.starts_with("delete ")),
        "{operations}"
    );
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    for (key, value) in before["states"].as_object().unwrap() {
        assert_eq!(&after["states"][key], value, "old resource changed");
    }
    let saved = fs::read_to_string(fixture.0.path().join("carina.state.json")).unwrap();
    assert!(
        saved.contains("valid-name"),
        "old resource must remain persisted"
    );
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
fn pending_inputs_saves_without_certificates_and_live_apply_discharges_before_create() {
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
    assert!(output.status.success(), "{}", text(&output));
    let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
    assert_eq!(
        operations
            .lines()
            .filter(|line| line.starts_with("create "))
            .collect::<Vec<_>>(),
        [
            "create test.resource.suffix",
            "create test.creation_constrained.pending"
        ],
        "{operations}",
    );
    let provider_state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        provider_state["states"]["test.creation_constrained.pending"]["name"],
        "suffix-id"
    );
    assert!(fixture.0.path().join("carina.state.json").exists());
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

fn save_pending_input_plan(fixture: &Fixture, producer_name: &str, replacement: bool) {
    fs::write(fixture.0.path().join("values.crn"), "").unwrap();
    let generation = if replacement { "generation = 2" } else { "" };
    fs::write(
        fixture.0.path().join("main.crn"),
        format!(
            "let producer = mock.test.resource {{ name = '{producer_name}' }}\n\
             let consumer = mock.test.estimated_input {{ name = producer.identifier {generation} }}\n"
        ),
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("input check pending until apply"),
        "{}",
        text(&output)
    );
    let saved = fs::read_to_string(fixture.0.path().join("plan.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(json["version"], 43);
    for field in [
        "pending_inputs",
        "PendingInputObligation",
        "InputCheckedPlan",
        "paired_delete",
        "certificate",
        "witness",
    ] {
        assert!(!saved.contains(field), "unexpected persisted {field}");
    }
}

#[test]
fn saved_input_readmission_rejects_tampered_create_before_effects_or_state_write() {
    tampered_saved_create_is_rejected(false);
}

#[test]
fn saved_input_readmission_rejection_precedes_legacy_state_migration_write() {
    tampered_saved_create_is_rejected(true);
}

fn tampered_saved_create_is_rejected(legacy_state: bool) {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let seed = mock.test.resource { name = 'seed' }\n",
    )
    .unwrap();
    let output = fixture.run(&["apply", "--auto-approve"]);
    assert!(output.status.success(), "{}", text(&output));
    if legacy_state {
        let path = fixture.0.path().join("carina.state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        state["version"] = serde_json::json!(20);
        fs::write(&path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    }
    let before = fs::read(fixture.0.path().join("carina.state.json")).unwrap();
    let provider_before = fs::read(fixture.0.path().join("mock-state.json")).unwrap();
    save_pending_input_plan(&fixture, "short", false);
    fs::write(fixture.0.path().join("op.log"), "").unwrap();
    let path = fixture.0.path().join("plan.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let create = plan["plan"]["effects"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter_map(|effect| effect.get_mut("Create"))
        .find(|create| create["id"]["resource_type"] == "test.estimated_input")
        .expect("pending create");
    let violating_value = "tampered-private-value-too-long-for-name";
    create["attributes"]["name"] = serde_json::json!({"String": violating_value});
    fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let plan_before = fs::read(&path).unwrap();

    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("cannot create"), "{}", text(&output));
    assert!(
        text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains(violating_value),
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
    assert_eq!(fs::read(&path).unwrap(), plan_before);
}

#[test]
fn saved_input_discharge_applies_valid_runtime_value() {
    let fixture = Fixture::new();
    save_pending_input_plan(&fixture, "short", false);
    let plan_before = fs::read(fixture.0.path().join("plan.json")).unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
    assert_eq!(
        operations
            .lines()
            .filter(|line| line.starts_with("create "))
            .collect::<Vec<_>>(),
        [
            "create test.resource.producer",
            "create test.estimated_input.consumer"
        ],
        "{operations}"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        state["states"]["test.estimated_input.consumer"]["name"],
        "short-id"
    );
    assert_eq!(
        fs::read(fixture.0.path().join("plan.json")).unwrap(),
        plan_before
    );
}

#[test]
fn saved_input_discharge_rejects_invalid_runtime_value_before_consumer_create() {
    let fixture = Fixture::new();
    save_pending_input_plan(&fixture, "long-apply-only-identifier", false);
    let plan_before = fs::read(fixture.0.path().join("plan.json")).unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("long-apply-only-identifier-id"),
        "{}",
        text(&output)
    );
    let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
    assert_eq!(
        operations
            .lines()
            .filter(|line| line.starts_with("create "))
            .collect::<Vec<_>>(),
        ["create test.resource.producer"],
        "{operations}"
    );
    assert_eq!(
        fs::read(fixture.0.path().join("plan.json")).unwrap(),
        plan_before
    );
}

#[test]
fn saved_input_discharge_runtime_violation_preserves_replaced_resource() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let consumer = mock.test.estimated_input { name = 'valid-name' generation = 1 }\n",
    )
    .unwrap();
    let output = fixture.run(&["apply", "--auto-approve"]);
    assert!(output.status.success(), "{}", text(&output));
    let before: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    save_pending_input_plan(&fixture, "long-apply-only-identifier", true);
    fs::write(fixture.0.path().join("op.log"), "").unwrap();

    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.name.length"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("long-apply-only-identifier-id"),
        "{}",
        text(&output)
    );
    let operations = fs::read_to_string(fixture.0.path().join("op.log")).unwrap();
    assert_eq!(
        operations
            .lines()
            .filter(|line| line.starts_with("create "))
            .collect::<Vec<_>>(),
        ["create test.resource.producer"],
        "{operations}"
    );
    assert!(
        !operations.lines().any(|line| line.starts_with("delete ")),
        "{operations}"
    );
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("mock-state.json")).unwrap())
            .unwrap();
    assert_eq!(
        after["states"]["test.estimated_input.consumer"],
        before["states"]["test.estimated_input.consumer"]
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.0.path().join("carina.state.json")).unwrap())
            .unwrap();
    let consumer = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|resource| resource["resource_type"] == "test.estimated_input")
        .unwrap();
    assert_eq!(consumer["current"]["attributes"]["name"], "valid-name");
}

#[test]
fn saved_input_readmission_checks_default_tags_before_pending_prerequisites() {
    let fixture = Fixture::new();
    fs::write(fixture.0.path().join("values.crn"), "").unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let producer = mock.test.resource { name = 'producer' }\n\
         let consumer = mock.test.input_constrained { name = 'valid-name' tags = producer.tags }\n",
    )
    .unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("input check pending until apply"),
        "{}",
        text(&output)
    );
    let path = fixture.0.path().join("plan.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    plan["provider_configs"][0]["default_tags"] =
        serde_json::json!({"aWs:private-default-key": {"String": "private-default-value"}});
    fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();

    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("mock.tags.reserved_prefix"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("aWs:private-default-key"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("private-default-value"),
        "{}",
        text(&output)
    );
    assert!(!fixture.0.path().join("op.log").exists());
    assert!(!fixture.0.path().join("carina.state.json").exists());
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn saved_input_readmission_rejects_tampered_update_without_state_write() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let consumer = mock.test.input_constrained { name = 'valid-name' tags = { owner = 'old' } }\n",
    ).unwrap();
    let output = fixture.run(&["apply", "--auto-approve"]);
    assert!(output.status.success(), "{}", text(&output));
    let before = fs::read(fixture.0.path().join("carina.state.json")).unwrap();
    let provider_before = fs::read(fixture.0.path().join("mock-state.json")).unwrap();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let consumer = mock.test.input_constrained { name = 'valid-name' tags = { owner = 'new' } }\n",
    ).unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    fs::write(fixture.0.path().join("op.log"), "").unwrap();
    let path = fixture.0.path().join("plan.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let update = plan["plan"]["effects"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find_map(|effect| effect.get_mut("Update"))
        .expect("saved update");
    update["to"]["attributes"]["tags"] =
        serde_json::json!({"Map": {"AWS:private-update-key": {"String": "private-update-value"}}});
    fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let plan_before = fs::read(&path).unwrap();

    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("cannot update"), "{}", text(&output));
    assert!(
        text(&output).contains("mock.tags.reserved_prefix"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("AWS:private-update-key"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("private-update-value"),
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
    assert_eq!(fs::read(&path).unwrap(), plan_before);
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

#[test]
fn saved_input_readmission_reports_drift_before_violating_value() {
    let fixture = Fixture::new();
    fs::write(
        fixture.0.path().join("main.crn"),
        "let seed = mock.test.resource { name = 'seed' comment = 'old' }\n",
    )
    .unwrap();
    let output = fixture.run(&["apply", "--auto-approve"]);
    assert!(output.status.success(), "{}", text(&output));
    fs::write(fixture.0.path().join("main.crn"), "let seed = mock.test.resource { name = 'seed' comment = 'old' }\nlet producer = mock.test.resource { name = 'short' }\nlet consumer = mock.test.estimated_input { name = producer.identifier }\n").unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    let path = fixture.0.path().join("plan.json");
    let mut plan: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let consumer = plan["plan"]["effects"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .filter_map(|effect| effect.get_mut("Create"))
        .find(|resource| resource["id"]["resource_type"] == "test.estimated_input")
        .unwrap();
    consumer["attributes"]["name"] = serde_json::json!({"String": "too-long-for-the-input-rule"});
    fs::write(&path, serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let mock_path = fixture.0.path().join("mock-state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&mock_path).unwrap()).unwrap();
    let seed = state["states"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .find(|resource| resource["name"] == "seed")
        .unwrap();
    seed["comment"] = serde_json::json!("drifted");
    fs::write(&mock_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();
    let before = fs::read(fixture.0.path().join("carina.state.json")).unwrap();
    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(
        text(&output).contains("Infrastructure drift detected"),
        "{}",
        text(&output)
    );
    assert!(
        !text(&output).contains("cannot create"),
        "{}",
        text(&output)
    );
    assert_eq!(
        fs::read(fixture.0.path().join("carina.state.json")).unwrap(),
        before
    );
}

#[test]
fn saved_input_unknown_module_output_remains_pending_until_dispatch() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.0.path().join("module")).unwrap();
    fs::write(fixture.0.path().join("module/main.crn"), "let source = mock.test.resource { name = 'short' }\nattributes { name = source.identifier }\n").unwrap();
    fs::write(fixture.0.path().join("main.crn"), "let module = use { source = './module' }\nlet output = module {}\nlet consumer = mock.test.estimated_input { name = output.name }\n").unwrap();
    let output = fixture.run(&["plan", "--refresh=false", "--out", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("input check pending until apply"),
        "{}",
        text(&output)
    );
    let output = fixture.run(&["apply", "--auto-approve", "plan.json"]);
    assert!(output.status.success(), "{}", text(&output));
    let state = fs::read_to_string(fixture.0.path().join("mock-state.json")).unwrap();
    assert!(state.contains("short-id"), "{state}");
}
