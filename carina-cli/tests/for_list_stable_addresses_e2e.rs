//! Directory-scoped regressions for value-keyed, let-bound list loops.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
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
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let scenario = Self {
            mock_state: project.join("mock-state.json"),
            operation_log: project.join("operations.log"),
            project,
            _temp: temp,
        };
        scenario.write(
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        scenario.write("providers.crn", "provider mock {}\n");
        scenario.write("resources.crn", "");
        success("init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn write(&self, file: &str, text: &str) {
        fs::write(self.project.join(file), text).expect("write configuration");
    }

    fn list(&self, items: &[&str]) {
        let items = items
            .iter()
            .map(|item| format!("'{item}'"))
            .collect::<Vec<_>>()
            .join(", ");
        self.write("iterable.crn", &format!("let items = [{items}]\n"));
        self.write(
            "resources.crn",
            "let rs = for x in items { mock.test.resource { name = \"payload-${x}\" } }\n",
        );
    }

    fn carina(&self, args: &[&str]) -> Output {
        self.carina_with(args, &[])
    }

    fn carina_with(&self, args: &[&str], controls: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .args(args)
            .envs(controls.iter().copied())
            .output()
            .expect("run carina")
    }

    fn apply(&self) {
        success(
            "apply",
            &self.carina(&["apply", "--auto-approve", "--lock=false", "."]),
        );
    }

    fn json(&self, file: &str) -> Value {
        serde_json::from_slice(&fs::read(self.project.join(file)).expect("read JSON"))
            .expect("parse JSON")
    }

    fn plan(&self) -> Value {
        success("plan", &self.carina(&["plan", "--out", "plan.json", "."]));
        self.json("plan.json")
    }

    fn rows(&self) -> BTreeMap<String, String> {
        self.json("carina.state.json")["resources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["binding"].as_str().unwrap().to_string(),
                    row["current"]["identifier"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn clear_operations(&self) {
        fs::write(&self.operation_log, "").expect("clear operation log");
    }

    fn mutations(&self) -> Vec<String> {
        fs::read_to_string(&self.operation_log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("create ") || line.starts_with("delete "))
            .map(str::to_owned)
            .collect()
    }
}

fn success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn effect_bindings(plan: &Value, kind: &str) -> Vec<String> {
    let mut bindings = plan["plan"]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|effect| effect.get(kind))
        .map(|effect| effect["id"]["identity"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    bindings.sort();
    bindings
}

#[test]
fn for_value_keys_map_group_dependencies() {
    let scenario = Scenario::new();
    scenario.write("iterable.crn", "let items = { a = 'alpha', b = 'beta' }\n");
    scenario.write(
        "resources.crn",
        "let rs = for k, v in items { mock.test.resource { name = v } }\n",
    );
    scenario.write(
        "references.crn",
        "let sink = mock.test.resource { name = 'sink' directives { depends_on = [rs] } }\n",
    );
    let plan = scenario.plan();
    let sink = plan["plan"]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|effect| effect.get("Create"))
        .find(|resource| resource["binding"] == "sink")
        .unwrap();
    assert_map_group_dependencies(&sink["explicit_dependencies"]);
    scenario.apply();
    let state = scenario.json("carina.state.json");
    let sink = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "sink")
        .unwrap();
    assert_map_group_dependencies(&sink["directives"]["depends_on"]);
}

fn assert_transition(before: &[&str], after: &[&str], creates: &[&str], deletes: &[&str]) {
    let scenario = Scenario::new();
    scenario.list(before);
    scenario.apply();
    let old_rows = scenario.rows();
    for item in before {
        assert!(old_rows.contains_key(&format!("rs.{item}")), "{old_rows:?}");
    }
    scenario.list(after);
    scenario.clear_operations();
    let plan = scenario.plan();
    assert_eq!(effect_bindings(&plan, "Create"), creates);
    assert_eq!(effect_bindings(&plan, "Delete"), deletes);
    assert!(effect_bindings(&plan, "Update").is_empty(), "{plan:#}");
    scenario.apply();
    let new_rows = scenario.rows();
    assert_eq!(new_rows.len(), after.len());
    let provider_state = scenario.json("mock-state.json");
    for item in after {
        let binding = format!("rs.{item}");
        let identifier = &new_rows[&binding];
        assert!(provider_state["states"][identifier].is_object());
        if let Some(old_identifier) = old_rows.get(&binding) {
            assert_eq!(identifier, old_identifier, "retained child {binding}");
        }
    }
    let operations = scenario.mutations();
    assert_eq!(
        operations
            .iter()
            .filter(|line| line.starts_with("create "))
            .count(),
        creates.len(),
        "{operations:?}",
    );
    assert_eq!(
        operations
            .iter()
            .filter(|line| line.starts_with("delete "))
            .count(),
        deletes.len(),
        "{operations:?}",
    );
}

#[test]
fn for_value_keys_remove_first() {
    assert_transition(&["a", "b", "c"], &["b", "c"], &[], &["rs.a"]);
}

#[test]
fn for_value_keys_insert_middle() {
    assert_transition(&["a", "c"], &["a", "b", "c"], &["rs.b"], &[]);
}

#[test]
fn for_value_keys_reorder() {
    assert_transition(&["a", "b", "c"], &["c", "a", "b"], &[], &[]);
}

#[test]
fn for_value_keys_append() {
    assert_transition(&["a", "b"], &["a", "b", "c"], &["rs.c"], &[]);
}

#[test]
fn for_value_keys_reference_stays_bound() {
    let scenario = Scenario::new();
    scenario.list(&["a", "b"]);
    scenario.write(
        "references.crn",
        "let sink = mock.test.resource { name = 'sink' parent_name = rs['b'].identifier }\n",
    );
    scenario.apply();
    let old_rows = scenario.rows();
    scenario.list(&["b", "a"]);
    scenario.clear_operations();
    let plan = scenario.plan();
    for kind in ["Create", "Update", "Delete"] {
        assert!(effect_bindings(&plan, kind).is_empty(), "{plan:#}");
    }
    scenario.apply();
    assert_eq!(scenario.rows(), old_rows);
    assert!(scenario.mutations().is_empty());
}

fn assert_rejected(output: &Output, message: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "unexpected success: {stdout}\n{stderr}"
    );
    assert!(
        format!("{stdout}\n{stderr}").contains(message),
        "missing {message}: {stdout}\n{stderr}"
    );
}

#[test]
fn for_value_keys_module_argument_duplicates_validate() {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("child")).unwrap();
    scenario.write("child/main.crn", "arguments { items: list(String) }\nlet rs = for x in items { mock.test.resource { name = x } }\n");
    scenario.write(
        "resources.crn",
        "let child = use { source = './child' }\nlet inst = child { items = ['MODULE_DUPLICATE_KEY_CANARY', 'MODULE_DUPLICATE_KEY_CANARY'] }\n",
    );
    let output = scenario.carina(&["validate", "."]);
    assert_rejected(
        &output,
        "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.",
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("MODULE_DUPLICATE_KEY_CANARY"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("MODULE_DUPLICATE_KEY_CANARY"));
    assert!(scenario.mutations().is_empty());
}

#[test]
fn for_value_keys_whole_secret_rejected_before_apply() {
    let scenario = Scenario::new();
    scenario.write(
        "resources.crn",
        "let rs = for x in secret(['WHOLE_KEY_CANARY']) { mock.test.resource { name = x } }\n",
    );
    for args in [vec!["validate", "."], vec!["plan", "."]] {
        let output = scenario.carina(&args);
        assert_rejected(
            &output,
            "Protected values cannot be value keys. Use `for (i, x) in ...`.",
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("WHOLE_KEY_CANARY"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("WHOLE_KEY_CANARY"));
    }
    assert!(scenario.mutations().is_empty());
}

#[test]
fn for_value_keys_reject_apply_phase_member() {
    let scenario = Scenario::new();
    scenario.write(
        "producer.crn",
        "let producer = mock.test.resource { name = 'producer' }\n",
    );
    scenario.write("iterable.crn", "let items = [producer.identifier, 'lit']\n");
    scenario.write(
        "resources.crn",
        "let rs = for x in items { mock.test.resource { name = x } }\n",
    );
    let output = scenario.carina(&["plan", "."]);
    assert_rejected(
        &output,
        "Element 1 is not known before apply. Use `for (i, x) in ...`.",
    );
    assert!(scenario.mutations().is_empty());
}

#[test]
fn for_value_keys_reject_authored_attribute_type() {
    let scenario = Scenario::new();
    scenario.write(
        "producer.crn",
        "let producer = mock.test.resource { name = 'producer' tags = { purpose = 'example' } }\n",
    );
    for iterable in [
        "[producer.tags]",
        "[producer.tags, 'TYPE_LABEL_CANARY']",
        "['TYPE_LABEL_CANARY', producer.tags]",
    ] {
        scenario.write(
            "resources.crn",
            &format!(
                "let rs = for x in {iterable} {{ mock.test.resource {{ name = 'constant' }} }}\n"
            ),
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(
            &output,
            "Element type Map cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("TYPE_LABEL_CANARY"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("TYPE_LABEL_CANARY"));
    }
}

fn generated_source(scenario: &Scenario) {
    scenario.write(
        "producer.crn",
        "let producer = mock.test.deferred_source { name = 'producer' }\n",
    );
    scenario.write(
        "resources.crn",
        "let rs = for x in producer.generated_strings { mock.test.resource { name = x } }\n",
    );
}

#[test]
fn for_value_keys_apply_failure_preserves_producer_state() {
    for saved in [false, true] {
        let scenario = Scenario::new();
        generated_source(&scenario);
        if saved {
            scenario.plan();
        }
        let args = if saved {
            vec!["apply", "--auto-approve", "--lock=false", "plan.json"]
        } else {
            vec!["apply", "--auto-approve", "--lock=false", "."]
        };
        let output =
            scenario.carina_with(&args, &[("CARINA_MOCK_GENERATED_STRINGS", "[\"a\",\"a\"]")]);
        assert_rejected(
            &output,
            "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.",
        );
        let rows = scenario.rows();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows.contains_key("producer"));
        assert!(scenario.json("mock-state.json")["states"][&rows["producer"]].is_object());
        assert_eq!(
            scenario.mutations().len(),
            1,
            "producer is the only mutation"
        );
    }
}

#[test]
fn for_value_keys_other_commands_same_error() {
    let scenario = Scenario::new();
    scenario.write(
        "producer.crn",
        "let producer = mock.test.deferred_source { name = 'producer' }\n",
    );
    success(
        "create producer",
        &scenario.carina_with(
            &["apply", "--auto-approve", "--lock=false", "."],
            &[("CARINA_MOCK_GENERATED_STRINGS", "[\"a\",\"a\"]")],
        ),
    );
    generated_source(&scenario);
    let before = fs::read(scenario.project.join("carina.state.json")).unwrap();
    let message = "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.";
    assert_rejected(&scenario.carina(&["state", "refresh", "."]), message);
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).unwrap(),
        before
    );
    let state = scenario.json("carina.state.json");
    let row = &state["resources"][0];
    let copied = serde_json::to_vec_pretty(&serde_json::json!({
        "version": 20, "exports_version": 1, "serial": 11,
        "lineage": "for-value-keys-copied-state", "carina_version": "legacy",
        "exports": {}, "resources": [{ "provider": "mock", "resource_type": "test.deferred_source", "identity": "producer", "binding": "producer", "current": row["current"] }]
    })).unwrap();
    fs::write(scenario.project.join("copied-v20.json"), &copied).unwrap();
    assert_rejected(
        &scenario.carina(&[
            "state",
            "migrate",
            "--check",
            ".",
            "--state-url",
            "copied-v20.json",
        ]),
        message,
    );
    assert_eq!(
        fs::read(scenario.project.join("copied-v20.json")).unwrap(),
        copied
    );
    assert_eq!(
        fs::read(scenario.project.join("carina.state.json")).unwrap(),
        before
    );
}

#[test]
fn for_value_keys_reject_empty_typed_module_argument() {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("child")).unwrap();
    scenario.write("child/main.crn", "arguments { items: list(Int) }\nlet rs = for x in items { mock.test.resource { name = 'constant' } }\n");
    scenario.write(
        "resources.crn",
        "let child = use { source = './child' }\nlet inst = child { items = [] }\n",
    );
    assert_rejected(
        &scenario.carina(&["validate", "."]),
        "Element type Int cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
    );
}

#[test]
fn for_value_keys_reject_eager_declared_element_types() {
    for (element_type, values, rejected_type) in [
        ("Bool", "[]", "Bool"),
        ("String | Int", "['FUNCTION_VALUE_CANARY']", "Int"),
    ] {
        let scenario = Scenario::new();
        scenario.write(
            "iterable.crn",
            &format!(
                "fn typed_items(): list({element_type}) {{ {values} }}\nlet items = typed_items()\n"
            ),
        );
        scenario.write("resources.crn", "mock.test.resource { name = 'control' }\n");
        success(
            "typed function without a loop",
            &scenario.carina(&["validate", "."]),
        );
        scenario.write(
            "resources.crn",
            "let rs = for (_, x) in items { mock.test.resource { name = 'constant' } }\n",
        );
        success(
            "indexed typed function result",
            &scenario.carina(&["validate", "."]),
        );
        scenario.write(
            "resources.crn",
            "let rs = for x in items { mock.test.resource { name = 'constant' } }\n",
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(
            &output,
            &format!(
                "Element type {rejected_type} cannot be a value key. Rewrite as strings or use `for (i, x) in ...`."
            ),
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("FUNCTION_VALUE_CANARY"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("FUNCTION_VALUE_CANARY"));
        assert!(scenario.mutations().is_empty());
    }
}

#[test]
fn for_value_keys_reject_provider_output_type() {
    let scenario = Scenario::new();
    scenario.write(
        "producer.crn",
        "let producer = mock.test.deferred_source { name = 'producer' }\n",
    );
    scenario.write(
        "resources.crn",
        "let rs = for x in producer.generated_rules { mock.test.resource { name = 'constant' } }\n",
    );
    assert_rejected(
        &scenario.carina(&["validate", "."]),
        "Element type Struct cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
    );
}

#[test]
fn for_value_keys_reject_upstream_element_type() {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("up")).unwrap();
    scenario.write("up/providers.crn", "provider mock {}\n");
    scenario.write("up/main.crn", "exports { items: list(Int) = [] }\n");
    scenario.write(
        "iterable.crn",
        "let up = upstream_state { source = './up' }\n",
    );
    scenario.write(
        "resources.crn",
        "let rs = for x in up.items { mock.test.resource { name = 'constant' } }\n",
    );
    assert_rejected(
        &scenario.carina(&["validate", "."]),
        "Element type Int cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.",
    );
}

#[test]
fn for_value_keys_upstream_authored_type_diagnostic() {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("up")).unwrap();
    scenario.write("up/providers.crn", "provider mock {}\n");
    scenario.write("up/exports.crn", "exports { items: list(Int) = [] }\n");
    scenario.write(
        "iterable.crn",
        "let up = upstream_state { source = './up' }\n",
    );
    let message =
        "Element type Int cannot be a value key. Rewrite as strings or use `for (i, x) in ...`.";
    for iterable in ["up.items", "(up.items ?? [1])"] {
        scenario.write(
            "resources.crn",
            &format!(
                "let rs = for x in {iterable} {{ mock.test.resource {{ name = 'constant' }} }}\n"
            ),
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(&output, message);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("resources.crn:1:19: {message}")),
            "{iterable}: {stderr}"
        );
        assert!(!stderr.contains("for-expression `"), "{stderr}");
        assert_eq!(stderr.matches(message).count(), 1, "{stderr}");
    }
}

fn partial_argument_module(item: &str) -> Scenario {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("child")).unwrap();
    scenario.write("child/arguments.crn", "arguments { item: String }\n");
    scenario.write(
        "child/resources.crn",
        "let inner = for y in [item, 'z'] { mock.test.resource { name = y } }\n",
    );
    scenario.write("modules.crn", "let child = use { source = './child' }\n");
    scenario.write(
        "resources.crn",
        &format!("let m = child {{ item = '{item}' }}\n"),
    );
    scenario
}

fn expression_argument_module(arguments: &str, iterable: &str, supplied: &str) -> Scenario {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("mod")).unwrap();
    scenario.write(
        "mod/main.crn",
        &format!(
            "{arguments}\nlet inner = for y in {iterable} {{ mock.test.resource {{ name = y }} }}\n"
        ),
    );
    scenario.write(
        "resources.crn",
        &format!("let m = use {{ source = './mod' }}\nlet inst = m {{ {supplied} }}\n"),
    );
    scenario
}

fn assert_module_expression_expands(
    arguments: &str,
    iterable: &str,
    supplied: &str,
    children: &[&str],
) {
    let scenario = expression_argument_module(arguments, iterable, supplied);
    scenario.write(
        "mod/sink.crn",
        "let sink = mock.test.resource { name = 'sink' comment = inner['z'].identifier }\n",
    );
    let output = scenario.carina(&["validate", "."]);
    success(
        "validate module iterable expression and sibling reference",
        &output,
    );
    let text = String::from_utf8_lossy(&output.stdout);
    for child in children {
        assert!(text.contains(child), "missing {child}: {text}");
    }
    assert!(!text.contains("inner[?]"), "{text}");

    let output = scenario.carina(&["plan", "--json", "."]);
    success(
        "plan module iterable expression and sibling reference",
        &output,
    );
    let plan: Value = serde_json::from_slice(&output.stdout).expect("JSON plan");
    let mut expected = children.to_vec();
    expected.push("inst.sink");
    expected.sort();
    assert_eq!(effect_bindings(&plan, "Create"), expected, "{plan:#}");
    let sink = plan["plan"]["effects"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|effect| effect.get("Create"))
        .find(|resource| resource["binding"] == "inst.sink")
        .expect("sibling sink");
    assert_eq!(
        sink["value_dependencies"][0]["identity"], "inst.inner.z",
        "{sink:#}"
    );
    assert!(
        sink["attributes"]["comment"]
            .to_string()
            .contains("inst.inner.z"),
        "{sink:#}"
    );
}

fn assert_module_iterable_runtime_error(output: &Output, function: &str) {
    assert!(
        !output.status.success(),
        "validate must reject runtime calls"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        format!(
            "Error: Module resolution error: Parse error in mod/main.crn: Invalid expression at line 0: for iterable function call '{function}' depends on a runtime value; all arguments must be statically known at parse time"
        ),
    );
}

fn assert_module_expression_rejected(
    arguments: &str,
    iterable: &str,
    supplied: &str,
    function: &str,
) {
    let scenario = expression_argument_module(arguments, iterable, supplied);
    assert_module_iterable_runtime_error(&scenario.carina(&["validate", "."]), function);
}

#[test]
fn for_value_keys_module_expression_interpolation() {
    assert_module_expression_expands(
        "arguments { prefix: String }",
        r#"["${prefix}-a", 'z']"#,
        "prefix = 'dev'",
        &["inst.inner['dev-a']", "inst.inner.z"],
    );
}

#[test]
fn for_value_keys_module_expression_concat() {
    assert_module_expression_rejected(
        "arguments { names: list(String) }",
        "concat(names, ['z'])",
        "names = ['a']",
        "concat",
    );
}

#[test]
fn for_value_keys_module_expression_member_function() {
    assert_module_expression_rejected(
        "arguments { item: String }",
        "[upper(item), 'z']",
        "item = 'a'",
        "upper",
    );
}

#[test]
fn for_value_keys_module_expression_split() {
    assert_module_expression_rejected(
        "arguments { csv: String }",
        "split(',', csv)",
        "csv = 'a,z'",
        "split",
    );
}

#[test]
fn for_value_keys_module_expression_keys() {
    assert_module_expression_rejected(
        "arguments { m: map(String) }",
        "keys(m)",
        "m = { a = 'first', z = 'last' }",
        "keys",
    );
}

#[test]
fn for_value_keys_module_runtime_concat_rejected_for_all_loop_forms() {
    for header in ["let o = for y", "for y", "let o = for (i, y)"] {
        let scenario = expression_argument_module(
            "arguments { names: list(String) }",
            "concat(names, ['z'])",
            "names = ['a']",
        );
        scenario.write(
            "mod/main.crn",
            &format!(
                "arguments {{ names: list(String) }}\n{header} in concat(names, ['z']) {{ mock.test.resource {{ name = y }} }}\n"
            ),
        );
        assert_module_iterable_runtime_error(&scenario.carina(&["validate", "."]), "concat");
    }
}

#[test]
fn for_value_keys_module_expression_bare_argument() {
    assert_module_expression_expands(
        "arguments { names: list(String) }",
        "names",
        "names = ['a', 'z']",
        &["inst.inner.a", "inst.inner.z"],
    );
}

#[test]
fn for_value_keys_module_expression_without_argument() {
    assert_module_expression_expands(
        "arguments { marker: String = 'unused' }",
        "concat(['a'], ['z'])",
        "",
        &["inst.inner.a", "inst.inner.z"],
    );
}

#[test]
fn for_value_keys_module_expression_provider_iterable_stays_deferred() {
    let scenario = expression_argument_module(
        "arguments { names: list(String) }",
        "names",
        "names = seed.generated_strings",
    );
    scenario.write(
        "seed.crn",
        "let seed = mock.test.deferred_source { name = 'seed' }\n",
    );
    let output = scenario.carina(&["validate", "."]);
    success("validate provider-backed module iterable", &output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("inst.inner[?]"));
    let output = scenario.carina(&["plan", "--json", "."]);
    success("plan provider-backed module iterable", &output);
    let plan: Value = serde_json::from_slice(&output.stdout).expect("JSON plan");
    assert_eq!(effect_bindings(&plan, "Create"), ["seed"]);
    assert!(
        plan["plan"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .any(|effect| effect.get("DeferredCreate").is_some()),
        "{plan:#}"
    );
    assert!(plan.to_string().contains("inst.inner[?]"), "{plan:#}");
}

fn assert_module_loop_caller_scope(partial: bool, shadowed: bool) {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("child")).unwrap();
    let (arguments, iterable, supplied, children) = if partial {
        (
            "item: String tag: String",
            "[item, 'z']",
            "item = 'a'",
            vec!["m.inner.a", "m.inner.z"],
        )
    } else {
        (
            "names: list(String) tag: String",
            "names",
            "names = ['a', 'b c']",
            vec!["m.inner.a", "m.inner['b c']"],
        )
    };
    scenario.write(
        "child/arguments.crn",
        &format!("arguments {{ {arguments} }}\n"),
    );
    let local = if shadowed {
        "let seed = mock.test.resource { name = 'inner-seed' }\n"
    } else {
        ""
    };
    scenario.write(
        "child/resources.crn",
        &format!("{local}let inner = for y in {iterable} {{ mock.test.resource {{ name = y comment = tag }} }}\n"),
    );
    scenario.write("modules.crn", "let child = use { source = './child' }\n");
    scenario.write(
        "resources.crn",
        &format!("let seed = mock.test.resource {{ name = 'seed' }}\nlet m = child {{ {supplied} tag = seed.identifier }}\n"),
    );
    success(
        "validate caller-scoped loop argument",
        &scenario.carina(&["validate", "."]),
    );
    let plan = scenario.plan();
    let creates = plan["plan"]["effects"].as_array().unwrap();
    for binding in children {
        let child = creates
            .iter()
            .filter_map(|effect| effect.get("Create"))
            .find(|resource| resource["binding"] == binding)
            .expect("loop child");
        let reference = child["attributes"]["comment"].to_string();
        assert!(reference.contains("\"seed\""), "{binding}: {child:#}");
        assert!(!reference.contains("m.seed"), "{binding}: {child:#}");
        assert_eq!(
            child["value_dependencies"].as_array().unwrap().len(),
            1,
            "{child:#}"
        );
        assert_eq!(
            child["value_dependencies"][0]["identity"], "seed",
            "{child:#}"
        );
    }
}

#[test]
fn for_value_keys_module_argument_caller_reference() {
    assert_module_loop_caller_scope(false, false);
}

#[test]
fn for_value_keys_module_argument_caller_reference_shadowed() {
    assert_module_loop_caller_scope(false, true);
}

#[test]
fn for_value_keys_module_partial_caller_reference() {
    assert_module_loop_caller_scope(true, false);
}

#[test]
fn for_value_keys_module_partial_caller_reference_shadowed() {
    assert_module_loop_caller_scope(true, true);
}

#[test]
fn for_value_keys_module_partial_list_validate() {
    let scenario = partial_argument_module("a");
    let output = scenario.carina(&["validate", "."]);
    success("validate partial module list", &output);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("m.inner.a"), "{text}");
    assert!(text.contains("m.inner.z"), "{text}");
    assert!(!text.contains("inner[?]"), "{text}");
}

#[test]
fn for_value_keys_module_partial_list_reference() {
    let scenario = partial_argument_module("a");
    scenario.write(
        "child/references.crn",
        "let sink = mock.test.resource { name = 'sink' comment = inner['z'].identifier }\n",
    );
    success(
        "validate module key reference",
        &scenario.carina(&["validate", "."]),
    );
    scenario.apply();
    let rows = scenario.rows();
    assert_eq!(
        rows.keys().cloned().collect::<Vec<_>>(),
        ["m.inner.a", "m.inner.z", "m.sink"]
    );
    let state = scenario.json("carina.state.json");
    let sink = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "m.sink")
        .unwrap();
    let child = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "m.inner.z")
        .unwrap();
    assert_eq!(
        sink["current"]["attributes"]["comment"].as_str(),
        Some(
            child["current"]["attributes"]["identifier"]
                .as_str()
                .unwrap()
        )
    );
    scenario.write(
        "child/references.crn",
        "let sink = mock.test.resource { name = 'sink' comment = inner[1].identifier }\n",
    );
    assert_rejected(
        &scenario.carina(&["validate", "."]),
        "This loop is value-keyed; use a string subscript, or `for (i, x) in ...` for index addressing.",
    );
}

#[test]
fn for_value_keys_module_partial_list_plan_group() {
    let scenario = partial_argument_module("a");
    scenario.write(
        "resources.crn",
        "let ms = for x in ['a'] { child { item = x } }\n",
    );
    let output = scenario.carina(&["plan", "."]);
    success("plan partial module list", &output);
    let text = String::from_utf8_lossy(&output.stdout);
    let group = text.split_once("module \"ms.a\"").expect(&text).1;
    let children = group
        .lines()
        .filter(|line| line.contains("mock.test.resource ms.a.inner."))
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2, "{text}");
    assert!(
        children.iter().any(|line| line.contains("ms.a.inner.a")),
        "{text}"
    );
    assert!(
        children.iter().any(|line| line.contains("ms.a.inner.z")),
        "{text}"
    );
    assert!(
        children
            .iter()
            .all(|line| line.contains("├─") || line.contains("└─")),
        "children must render inside the module tree: {text}"
    );
}

#[test]
fn for_value_keys_module_partial_list_duplicate() {
    let scenario = partial_argument_module("z");
    assert_rejected(
        &scenario.carina(&["validate", "."]),
        "Elements 1 and 2 produce the same key. Use `for (i, x) in ...`.",
    );
}

#[test]
fn for_value_keys_deferred_key_location_once() {
    let scenario = Scenario::new();
    fs::create_dir(scenario.project.join("child")).unwrap();
    scenario.write("child/arguments.crn", "arguments { items: list(String) }\n");
    scenario.write(
        "child/resources.crn",
        "let rs = for x in items { mock.test.resource { name = x } }\n",
    );
    scenario.write(
        "resources.crn",
        "let child = use { source = './child' }\nlet m = child { items = ['a', 'b', 'a'] }\n",
    );
    let output = scenario.carina(&["validate", "."]);
    let message = "Elements 1 and 3 produce the same key. Use `for (i, x) in ...`.";
    assert_rejected(&output, message);
    let text = String::from_utf8_lossy(&output.stderr);
    let line = text
        .lines()
        .find(|line| line.contains(message))
        .expect(&text);
    let suffix = line.split_once("resources.crn:1:19: ").expect(line).1;
    assert_eq!(suffix, message, "{text}");
}

#[test]
fn for_value_keys_apply_time_expansion_roundtrip() {
    let scenario = Scenario::new();
    scenario.write(
        "producer.crn",
        "let producer = mock.test.deferred_source { name = 'producer' }\n",
    );
    scenario.write("resources.crn", "let rs = for x in producer.generated_strings { mock.test.resource { name = \"payload-${x}\" } }\n");
    success(
        "apply generated keys",
        &scenario.carina_with(
            &["apply", "--auto-approve", "--lock=false", "."],
            &[("CARINA_MOCK_GENERATED_STRINGS", "[\"a\",\"b c\"]")],
        ),
    );
    let before = scenario.rows();
    assert_eq!(
        before.keys().cloned().collect::<Vec<_>>(),
        ["producer", "rs.a", "rs['b c']"]
    );
    scenario.clear_operations();
    let plan = scenario.plan();
    assert!(
        plan["plan"]["effects"].as_array().unwrap().is_empty(),
        "{plan}"
    );
    assert_eq!(scenario.rows(), before);
    assert!(scenario.mutations().is_empty());
}

#[test]
fn for_value_keys_reject_non_roundtripping_keys() {
    let scenario = Scenario::new();
    for iterable in ["['a[b]']", "[\"q'].z['w\"]"] {
        scenario.write(
            "resources.crn",
            &format!(
                "let rs = for x in {iterable} {{ mock.test.resource {{ name = 'constant' }} }}\n"
            ),
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(
            &output,
            "Element 1 cannot be used as an address key. Use `for (i, x) in ...`.",
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !text.contains("a[b]") && !text.contains("q'].z['w"),
            "{text}"
        );
        assert!(scenario.mutations().is_empty());
    }
}

#[test]
fn for_value_keys_plan_phase_member_retries() {
    let upstream = Scenario::new();
    upstream.write(
        "resources.crn",
        "let seed = mock.test.resource { name = 'seed' }\n",
    );
    upstream.write("exports.crn", "exports { item: String = 'remote' }\n");
    upstream.apply();
    let scenario = Scenario::new();
    scenario.write(
        "upstream.crn",
        &format!(
            "let up = upstream_state {{ source = '{}' }}\n",
            upstream.project.display()
        ),
    );
    scenario.write("iterable.crn", "let items = [up.item, 'lit']\n");
    scenario.write(
        "resources.crn",
        "let rs = for x in items { mock.test.resource { name = \"payload-${x}\" } }\n",
    );
    assert_eq!(
        effect_bindings(&scenario.plan(), "Create"),
        ["rs.lit", "rs.remote"]
    );
    scenario.apply();
    assert_eq!(
        scenario.rows().keys().cloned().collect::<Vec<_>>(),
        ["rs.lit", "rs.remote"]
    );
}

#[test]
fn for_value_keys_reject_newly_deferred_bodies() {
    for body in ["read mock.test.resource { name = x }", "child { item = x }"] {
        let scenario = Scenario::new();
        fs::create_dir(scenario.project.join("child")).unwrap();
        scenario.write(
            "child/main.crn",
            "arguments { item: String }\nlet resource = mock.test.resource { name = item }\n",
        );
        scenario.write("producer.crn", "let producer = mock.test.resource { name = 'producer' }\nlet child = use { source = './child' }\n");
        scenario.write(
            "iterable.crn",
            "let items = [producer.identifier, 'BODY_KEY_CANARY']\n",
        );
        scenario.write(
            "resources.crn",
            &format!("let rs = for x in items {{ {body} }}\n"),
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(&output, "cannot use");
        assert_rejected(&output, "`for (i, x) in ...`");
        assert_rejected(
            &output,
            "`for x in [producer.identifier, 'BODY_KEY_CANARY']`",
        );
    }
}

#[test]
fn for_value_keys_indexed_opt_in() {
    for input in [
        "[1, 2]",
        "[true, false]",
        "[{ action = 'a' }, { action = 'b' }]",
        "['same', 'same']",
        "[secret('INDEX_MEMBER_CANARY'), secret('INDEX_MEMBER_CANARY')]",
    ] {
        let scenario = Scenario::new();
        scenario.write("iterable.crn", &format!("let items = {input}\n"));
        scenario.write(
            "resources.crn",
            "let rs = for (i, _) in items { mock.test.resource { name = \"payload-${i}\" } }\n",
        );
        scenario.apply();
        assert_eq!(
            scenario.rows().keys().cloned().collect::<Vec<_>>(),
            ["rs[0]", "rs[1]"]
        );
    }
}

fn indexed_state(scenario: &Scenario) -> String {
    scenario.write("iterable.crn", "let items = ['a']\n");
    scenario.write(
        "resources.crn",
        "let rs = for (i, x) in items { mock.test.resource { name = x } }\n",
    );
    scenario.apply();
    scenario.rows()["rs[0]"].clone()
}

#[test]
fn for_value_keys_existing_index_state_requires_moves() {
    let scenario = Scenario::new();
    let identifier = indexed_state(&scenario);
    scenario.write(
        "resources.crn",
        "let rs = for x in items { mock.test.resource { name = x } }\n",
    );
    let output = scenario.carina(&["plan", "--out", "plan.json", "."]);
    success("address-breaking plan", &output);
    let plan = scenario.json("plan.json");
    assert_eq!(effect_bindings(&plan, "Delete"), ["rs[0]"]);
    assert_eq!(effect_bindings(&plan, "Create"), ["rs.a"]);
    let warning = String::from_utf8_lossy(&output.stderr);
    assert!(
        warning.contains("moved {") && warning.contains("rs[0]") && warning.contains("rs.a"),
        "{warning}"
    );
    scenario.write(
        "moves.crn",
        "moved { from = mock.test.resource 'rs[0]' to = mock.test.resource 'rs.a' }\n",
    );
    let plan = scenario.plan();
    assert!(effect_bindings(&plan, "Delete").is_empty(), "{plan:#}");
    assert!(effect_bindings(&plan, "Create").is_empty(), "{plan:#}");
    scenario.clear_operations();
    scenario.apply();
    assert_eq!(
        scenario.rows(),
        BTreeMap::from([("rs.a".into(), identifier)])
    );
    assert!(scenario.mutations().is_empty());
}

#[test]
fn for_value_keys_map_group_state_dependencies() {
    for saved_plan in [false, true] {
        let scenario = Scenario::new();
        scenario.write("iterable.crn", "let items = { a = 'alpha', b = 'beta' }\n");
        scenario.write(
            "resources.crn",
            "let rs = for k, v in items { mock.test.resource { name = v } }\n",
        );
        scenario.write(
            "references.crn",
            "let sink = mock.test.resource { name = 'sink' directives { depends_on = [rs] } }\n",
        );
        let plan = scenario.plan();
        let sink = plan["plan"]["effects"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|effect| effect.get("Create"))
            .find(|resource| resource["binding"] == "sink")
            .unwrap();
        assert_map_group_dependencies(&sink["explicit_dependencies"]);
        if saved_plan {
            success(
                "apply saved map-group dependencies",
                &scenario.carina(&["apply", "--auto-approve", "--lock=false", "plan.json"]),
            );
        } else {
            scenario.apply();
        }
        let state = scenario.json("carina.state.json");
        let sink = state["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["binding"] == "sink")
            .unwrap();
        assert_map_group_dependencies(&sink["directives"]["depends_on"]);
    }
}

fn assert_map_group_dependencies(dependencies: &Value) {
    let mut actual = dependencies
        .as_array()
        .expect("explicit dependency set")
        .clone();
    actual.sort_by_key(|dependency| dependency["identity"].as_str().unwrap().to_string());
    assert_eq!(
        Value::Array(actual),
        serde_json::json!([
            { "provider": "mock", "resource_type": "test.resource", "identity": "rs.a" },
            { "provider": "mock", "resource_type": "test.resource", "identity": "rs.b" },
        ]),
    );
}

#[test]
fn for_value_keys_reject_deferred_declared_element_types() {
    for (element_type, values, rejected_type) in [
        ("Bool", "[]", "Bool"),
        ("String | Int", "['FUNCTION_VALUE_CANARY']", "Int"),
    ] {
        let scenario = Scenario::new();
        fs::create_dir(scenario.project.join("child")).unwrap();
        scenario.write(
            "child/main.crn",
            &format!("arguments {{ items: list({element_type}) }}\nlet rs = for x in items {{ mock.test.resource {{ name = 'constant' }} }}\n"),
        );
        scenario.write(
            "iterable.crn",
            &format!(
                "fn typed_items(): list({element_type}) {{ {values} }}\nlet items = typed_items()\n"
            ),
        );
        scenario.write(
            "resources.crn",
            "let child = use { source = './child' }\nlet inst = child { items = items }\n",
        );
        let output = scenario.carina(&["validate", "."]);
        assert_rejected(
            &output,
            &format!(
                "Element type {rejected_type} cannot be a value key. Rewrite as strings or use `for (i, x) in ...`."
            ),
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("FUNCTION_VALUE_CANARY"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("FUNCTION_VALUE_CANARY"));
        assert!(scenario.mutations().is_empty());
    }
}
