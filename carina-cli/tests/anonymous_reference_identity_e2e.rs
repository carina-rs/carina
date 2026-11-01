//! Multi-file regression coverage for reference-valued declared identity.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create reference-identity project");
        let project = temp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/anonymous_reference_identity");
        for entry in fs::read_dir(fixture).expect("read reference-identity fixture") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
        Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-operations.log"),
            project,
            _temp: temp,
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn init(&self) {
        assert_success("carina init", &self.carina(&["init", "."]));
    }

    fn state_json(&self) -> Value {
        read_json(&self.project.join("carina.state.json"))
    }

    fn record_identities(&self) -> BTreeSet<String> {
        self.state_json()["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .filter(|row| row["resource_type"] == "route53.RecordSet")
            .map(|row| {
                row["identity"]
                    .as_str()
                    .expect("record identity")
                    .to_string()
            })
            .collect()
    }

    fn rewrite_record_address(&self, name: &str, old_identity: &str) -> String {
        let state_path = self.project.join("carina.state.json");
        let mut state = read_json(&state_path);
        let rows = state["resources"].as_array_mut().expect("state resources");
        let row_index = rows
            .iter()
            .position(|row| {
                row["resource_type"] == "route53.RecordSet"
                    && row
                        .pointer("/current/attributes/name")
                        .and_then(Value::as_str)
                        == Some(name)
                    && row
                        .pointer("/current/attributes/type")
                        .and_then(Value::as_str)
                        == Some("A")
            })
            .unwrap_or_else(|| panic!("missing A record {name}: {rows:#?}"));
        let row = &mut rows[row_index];
        let new_identity = row["identity"]
            .as_str()
            .expect("record identity")
            .to_string();
        row["identity"] = Value::String(old_identity.to_string());
        write_json(&state_path, &state);

        let mut provider_state = read_json(&self.mock_state);
        let states = provider_state["states"]
            .as_object_mut()
            .expect("mock provider states");
        let new_key = format!("route53.RecordSet.{new_identity}");
        let attributes = states
            .remove(&new_key)
            .unwrap_or_else(|| panic!("missing mock provider row {new_key}: {states:#?}"));
        states.insert(format!("route53.RecordSet.{old_identity}"), attributes);
        write_json(&self.mock_state, &provider_state);
        new_identity
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output),
    );
}

fn planned_record_identities(output: &Output) -> BTreeSet<String> {
    let marker = "mock.route53.RecordSet ";
    output_text(output)
        .lines()
        .filter_map(|line| {
            line.split_once(marker)?
                .1
                .split_whitespace()
                .next()
                .filter(|identity| identity.starts_with("mock_route53_record_set_"))
                .map(str::to_string)
        })
        .collect()
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize JSON fixture"),
    )
    .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn legacy_reference_record_identity(name: &str, record: &str) -> String {
    let mut inputs = BTreeMap::new();
    inputs.insert("hosted_zone_id", "ResourceRef(zone.id)".to_string());
    inputs.insert("name", format!("String({name:?})"));
    inputs.insert("resource_records", format!("List([String({record:?})])"));
    inputs.insert("type", "String(\"A\")".to_string());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in inputs {
        key.hash(&mut hasher);
        value.hash(&mut hasher);
    }
    format!(
        "mock_route53_record_set_{:08x}",
        hasher.finish() & 0xffff_ffff
    )
}

#[test]
fn reference_identity_addresses_are_stable_across_plan_apply_plan() {
    let scenario = Scenario::new();
    scenario.init();
    assert_success("carina validate", &scenario.carina(&["validate", "."]));

    let initial_plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("initial carina plan", &initial_plan);
    let planned = planned_record_identities(&initial_plan);
    assert_eq!(
        planned.len(),
        3,
        "initial plan:\n{}",
        output_text(&initial_plan)
    );

    assert_success(
        "carina apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    assert_eq!(
        scenario.record_identities(),
        planned,
        "apply-time resolution of zone.id must retain every planned address",
    );

    let converged = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("converged carina plan", &converged);
    assert!(
        output_text(&converged).contains("No changes. Infrastructure is up-to-date."),
        "post-apply plan must converge at the reference-derived addresses:\n{}",
        output_text(&converged),
    );
}

#[test]
fn duplicate_name_and_type_in_the_same_referenced_zone_is_rejected() {
    let scenario = Scenario::new();
    let records_path = scenario.project.join("records.crn");
    let mut records = fs::read_to_string(&records_path).expect("read records fixture");
    records.push_str(
        r#"
mock.route53.RecordSet {
  hosted_zone_id   = zone.id
  name             = "www.example.com."
  type             = "A"
  resource_records = [
    "192.0.2.99",
  ]
}
"#,
    );
    fs::write(records_path, records).expect("append duplicate record");
    scenario.init();

    for args in [
        ["validate", "."].as_slice(),
        ["plan", "--refresh=false", "."].as_slice(),
    ] {
        let output = scenario.carina(args);
        let rendered = output_text(&output);
        assert!(
            !output.status.success()
                && rendered.contains("equal declared identity values")
                && rendered.contains("name with `let`"),
            "carina {} must reject the duplicate record:\n{rendered}",
            args[0],
        );
    }
}

#[test]
fn legacy_create_only_reference_hash_is_moved_to_the_declared_identity_address() {
    let scenario = Scenario::new();
    scenario.init();
    assert_success(
        "initial carina apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    let old_identity = legacy_reference_record_identity("www.example.com.", "192.0.2.10");
    let new_identity = scenario.rewrite_record_address("www.example.com.", &old_identity);
    assert_ne!(
        old_identity, new_identity,
        "fixture must exercise an address move"
    );

    let migration = scenario.carina(&[
        "plan",
        "--refresh=false",
        "--out",
        "reference-identity-migration.plan.json",
        ".",
    ]);
    assert_success("reference identity migration plan", &migration);
    let rendered = output_text(&migration);
    assert!(
        rendered.contains(&format!("moved from: {old_identity}"))
            && rendered.contains(&new_identity)
            && rendered.contains("1 to move"),
        "legacy reference hash must migrate as one move:\n{rendered}",
    );

    fs::write(&scenario.operation_log, "").expect("clear mock operation log");
    assert_success(
        "reference identity migration apply",
        &scenario.carina(&[
            "apply",
            "--auto-approve",
            "--lock=false",
            "reference-identity-migration.plan.json",
        ]),
    );
    assert!(scenario.record_identities().contains(&new_identity));
    let operations = fs::read_to_string(&scenario.operation_log).unwrap_or_default();
    assert!(
        operations.lines().all(|line| {
            !line.starts_with("create ")
                && !line.starts_with("update ")
                && !line.starts_with("delete ")
        }),
        "the address migration must not mutate provider objects:\n{operations}",
    );

    let converged = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("post-migration plan", &converged);
    assert!(
        output_text(&converged).contains("No changes. Infrastructure is up-to-date."),
        "the migrated address must converge:\n{}",
        output_text(&converged),
    );
}

#[test]
fn hosted_zone_replacement_with_unrelated_record_plans_delete_and_create() {
    let scenario = Scenario::new();
    fs::write(
        scenario.project.join("records.crn"),
        r#"mock.route53.RecordSet {
  hosted_zone_id   = zone.id
  name             = "old.example.com."
  type             = "A"
  resource_records = ["192.0.2.10"]
}
"#,
    )
    .expect("write initial record fixture");
    scenario.init();
    assert_success(
        "initial carina apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );

    fs::write(
        scenario.project.join("zone.crn"),
        "let new_zone = mock.route53.HostedZone { name = \"new.example.com.\" }\n",
    )
    .expect("write replacement hosted zone");
    fs::write(
        scenario.project.join("records.crn"),
        r#"mock.route53.RecordSet {
  hosted_zone_id   = new_zone.id
  name             = "new.example.com."
  type             = "A"
  resource_records = ["192.0.2.20"]
}
"#,
    )
    .expect("write replacement record fixture");

    let replacement = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("hosted-zone replacement plan", &replacement);
    let rendered = output_text(&replacement);
    assert!(
        rendered.contains("Plan: 2 to add, 0 to change, 2 to destroy."),
        "the old zone and record must be deleted while their unrelated replacements are created:\n\
         {rendered}",
    );
    assert!(
        !rendered.contains("to move"),
        "unexpected migration:\n{rendered}"
    );
}
