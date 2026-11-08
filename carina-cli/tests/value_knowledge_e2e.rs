//! Directory regressions that must survive the value-knowledge migration.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect as Effect;
use carina_core::resource::{ConcreteValue, Value};

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env(
            "CARINA_MOCK_STATE_FILE",
            dir.join("mock-provider-state.json"),
        )
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_4316_LAZY_MISSING")
        .args(args)
        .output()
        .expect("run carina")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn fixture_files(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/value_knowledge")
        .join(name);
    for file in fs::read_dir(source).unwrap() {
        let file = file.unwrap();
        fs::copy(file.path(), dir.path().join(file.file_name())).unwrap();
    }
    dir
}

fn fixture(name: &str) -> tempfile::TempDir {
    let dir = fixture_files(name);
    success(&run(dir.path(), &["init", "."]));
    success(&run(dir.path(), &["validate", "."]));
    dir
}

fn plan(dir: &Path) -> PlanFile {
    let output = run(dir, &["plan", "--json", "."]);
    success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout[stdout.find('{').expect("plan JSON")..]).unwrap()
}

fn names(plan: &PlanFile) -> Vec<String> {
    let mut names = plan
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Create(resource) => match resource.as_resource().get_attr("name") {
                Some(Value::Concrete(ConcreteValue::String(name))) => {
                    Some(name.expose_plaintext().to_owned())
                }
                other => panic!("expected concrete child name: {other:?}"),
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn lexical_resource_function_iterable_plans_in_a_directory() {
    let fixture = fixture("lexical_resource");
    assert_eq!(names(&plan(fixture.path())), ["pre-r-x", "r"]);
}

#[test]
fn sibling_lexical_function_iterable_plans_in_a_directory() {
    let fixture = fixture("lexical_sibling");
    assert_eq!(names(&plan(fixture.path())), ["pre-a"]);
}

#[test]
fn unknown_collection_keeps_one_deferred_create_instead_of_zero_children() {
    let fixture = fixture("unknown_count");
    let plan = plan(fixture.path());
    assert_eq!(names(&plan), ["producer"]);
    assert_eq!(
        plan.plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::DeferredCreate(_)))
            .count(),
        1,
    );
    let rendered = run(fixture.path(), &["plan", "."]);
    success(&rendered);
    let stdout = String::from_utf8_lossy(&rendered.stdout);
    assert!(
        stdout.contains("  + mock.test.resource producer"),
        "{stdout}"
    );
    assert!(
        stdout.contains("└─ + mock.test.resource (N records after producer applies)"),
        "{stdout}"
    );
    assert_eq!(
        stdout.matches("+ mock.test.resource").count(),
        2,
        "{stdout}"
    );
    assert!(
        stdout.contains("Plan: 1 to add, 0 to change, 0 to destroy."),
        "{stdout}"
    );
    assert!(
        stdout.contains("N to add after producer applies."),
        "{stdout}"
    );
}

#[test]
fn saved_plan_keeps_sibling_lexical_calls_for_an_apply_only_argument() {
    let fixture = fixture("lexical_apply");
    success(&run(fixture.path(), &["plan", "--out", "plan.json", "."]));
    let saved = fs::read_to_string(fixture.path().join("plan.json")).unwrap();
    let saved_plan: PlanFile = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        saved_plan
            .plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::Create(_)))
            .count(),
        2,
        "the apply-only argument must preserve both managed resources",
    );
    success(&run(
        fixture.path(),
        &["apply", "--auto-approve", "--lock=false", "plan.json"],
    ));
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("mock-provider-state.json")).unwrap(),
    )
    .unwrap();
    let resources = state["states"].as_object().unwrap();
    let producer = resources
        .values()
        .find(|attributes| attributes["name"] == "producer")
        .unwrap();
    let expected = format!("pre-{}", producer["identifier"].as_str().unwrap());
    assert_eq!(resources.len(), 2);
    assert!(
        resources
            .values()
            .any(|attributes| attributes["name"] == expected)
    );
    assert!(plan(fixture.path()).plan.effects().is_empty());
}

#[test]
fn saved_plan_keeps_an_unselected_failing_fallback_lazy() {
    let fixture = fixture("lazy_fallback");
    success(&run(fixture.path(), &["plan", "--out", "plan.json", "."]));
    let saved = fs::read_to_string(fixture.path().join("plan.json")).unwrap();
    assert!(saved.contains("CARINA_4316_LAZY_MISSING"));
    success(&run(
        fixture.path(),
        &["apply", "--auto-approve", "--lock=false", "plan.json"],
    ));
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixture.path().join("mock-provider-state.json")).unwrap(),
    )
    .unwrap();
    let resources = state["states"].as_object().unwrap();
    let by_name = |name: &str| {
        resources
            .values()
            .find(|attributes| attributes["name"] == name)
            .unwrap()
    };
    assert_eq!(
        by_name("consumer")["comment"],
        by_name("producer")["identifier"],
    );
    assert!(plan(fixture.path()).plan.effects().is_empty());
}

fn assert_partial_children(plan: &PlanFile, count: usize) {
    assert_eq!(
        plan.plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::Create(_)))
            .count(),
        count + 2
    );
    assert!(plan.plan.effects().iter().all(|effect| !matches!(
        effect,
        Effect::DeferredCreate(_) | Effect::DeferredReplace(_)
    )));
    let effects = plan.plan.effects();
    assert_eq!(
        effects.len(),
        count + 2,
        "one ordinary effect per child and producer"
    );
    let mut children = 0;
    for effect in effects {
        let Effect::Create(resource) = effect else {
            panic!("ordinary create: {effect:?}");
        };
        let resource = resource.as_resource();
        if let Some(Value::Concrete(ConcreteValue::String(name))) = resource.get_attr("name")
            && name.expose_plaintext().starts_with("child-")
        {
            children += 1;
            let parent = resource.get_attr("parent_name").unwrap();
            assert!(
                matches!(parent, Value::Deferred(_)),
                "leaf still awaits producer publication"
            );
            assert!(
                parent
                    .root_bindings()
                    .iter()
                    .any(|binding| binding.as_str().starts_with("producer_")),
                "original dependency selector: {parent:?}"
            );
        }
    }
    assert_eq!(children, count);
}

#[test]
fn direct_partial_collections_expand_zero_one_and_many_children() {
    for count in [0, 1, 3] {
        let fixture = fixture("partial_direct");
        let items = (0..count)
            .map(|index| {
                if index % 2 == 0 {
                    "producer_a.identifier"
                } else {
                    "producer_b.identifier"
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(fixture.path().join("main.crn"), format!(
            "let producer_a = mock.test.resource {{ name = \"producer-a\" }}\nlet producer_b = mock.test.resource {{ name = \"producer-b\" }}\nfor (i, item) in [{items}] {{ mock.test.resource {{ name = \"child-${{i}}\" parent_name = item }} }}\n"
        )).unwrap();
        assert_partial_children(&plan(fixture.path()), count);
    }
}

#[test]
fn absent_upstream_fallback_expands_partial_shape_after_resolution() {
    // Unlike the direct literal case, parser-time membership is unknown here.
    let fixture = fixture_files("partial_coalesced");
    let up = fixture.path().join("up");
    fs::create_dir(&up).unwrap();
    fs::write(
        up.join("main.crn"),
        "let seed = mock.test.resource { name = \"seed\" }\n",
    )
    .unwrap();
    fs::write(up.join("providers.crn"), "provider mock {}\n").unwrap();
    fs::write(up.join("exports.crn"), "exports {}\n").unwrap();
    fs::write(
        up.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .unwrap();
    success(&run(&up, &["init", "."]));
    success(&run(&up, &["apply", "--auto-approve", "."]));
    success(&run(fixture.path(), &["init", "."]));
    success(&run(fixture.path(), &["validate", "."]));
    assert_partial_children(&plan(fixture.path()), 2);
}

#[test]
fn flattened_partial_shapes_expand_real_plan_children() {
    let fixture = fixture("shape_functions");
    assert_partial_children(&plan(fixture.path()), 2);
}

#[test]
fn partial_shape_replaces_existing_children_once_without_retaining_a_template() {
    let fixture = fixture("partial_direct");
    let path = fixture.path().join("main.crn");
    let original = fs::read_to_string(&path)
        .unwrap()
        .replace("for (i, item)", "let children = for (i, item)");
    fs::write(&path, &original).unwrap();
    success(&run(
        fixture.path(),
        &["apply", "--auto-approve", "--lock=false", "."],
    ));
    assert!(plan(fixture.path()).plan.effects().is_empty());
    fs::write(&path, original.replace("child-${i}", "next-child-${i}")).unwrap();
    let changed = plan(fixture.path());
    assert_eq!(changed.plan.effects().len(), 4);
    assert_eq!(
        changed
            .plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::Create(_)))
            .count(),
        2
    );
    assert_eq!(
        changed
            .plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::Delete { .. }))
            .count(),
        2
    );
    assert!(changed.plan.effects().iter().all(|effect| !matches!(
        effect,
        Effect::DeferredCreate(_) | Effect::DeferredReplace(_)
    )));
}

#[test]
fn present_authored_partial_map_keeps_its_shape_after_coalesced_reference_selection() {
    let fixture = fixture("partial_direct");
    fs::write(fixture.path().join("main.crn"),r#"
let producer_a = mock.test.resource { name = "producer-a" }
let producer_b = mock.test.resource { name = "producer-b" }
let carrier = mock.test.resource { name = "carrier" tags = {left = producer_a.identifier, right = producer_b.identifier} }
for key, item in (carrier.tags ?? {}) {
  mock.test.resource { name = "child-${key}" parent_name = item }
}
"#).unwrap();
    let result = plan(fixture.path());
    assert_eq!(
        names(&result),
        [
            "carrier",
            "child-left",
            "child-right",
            "producer-a",
            "producer-b"
        ]
    );
    assert_eq!(result.plan.effects().len(), 5);
    assert!(
        result
            .plan
            .effects()
            .iter()
            .all(|effect| matches!(effect, Effect::Create(_)))
    );
}

mod protected_operations {
    use carina_cli::commands::plan::PlanFile;
    use carina_core::effect::UnresolvedSavedEffect as Effect;
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };

    fn run(dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(
                "CARINA_MOCK_STATE_FILE",
                dir.join("mock-provider-state.json"),
            )
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .unwrap()
    }
    fn success(output: &Output) {
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn fixture(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/value_knowledge")
            .join(name);
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
        success(&run(dir.path(), &["init", "."]));
        success(&run(dir.path(), &["validate", "."]));
        dir
    }
    fn plan(name: &str) -> PlanFile {
        let dir = fixture(name);
        let output = run(dir.path(), &["plan", "--out", "plan.json", "."]);
        success(&output);
        serde_json::from_str(&fs::read_to_string(dir.path().join("plan.json")).unwrap()).unwrap()
    }
    #[test]
    fn protected_interpolation_keeps_apply_timing() {
        assert_eq!(
            plan("protected_interpolation")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            2
        );
    }
    #[test]
    fn protected_builtin_keeps_apply_timing() {
        assert_eq!(
            plan("protected_builtin")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            2
        );
    }
    #[test]
    fn protected_pending_iterable_stays_deferred() {
        assert_eq!(
            plan("protected_loop")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::DeferredCreate(_)))
                .count(),
            1
        );
    }
}

mod evaluation_timing {
    use carina_cli::commands::plan::PlanFile;
    use carina_core::effect::UnresolvedSavedEffect as Effect;
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };

    fn run(dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(
                "CARINA_MOCK_STATE_FILE",
                dir.join("mock-provider-state.json"),
            )
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .unwrap()
    }
    fn success(output: &Output) {
        assert!(
            output.status.success(),
            "stdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn fixture(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/value_knowledge")
            .join(name);
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
        success(&run(dir.path(), &["init", "."]));
        success(&run(dir.path(), &["validate", "."]));
        dir
    }
    fn plan(name: &str) -> PlanFile {
        let dir = fixture(name);
        let output = run(dir.path(), &["plan", "--out", "plan.json", "."]);
        success(&output);
        serde_json::from_str(&fs::read_to_string(dir.path().join("plan.json")).unwrap()).unwrap()
    }

    #[test]
    fn pending_secret_parameter_remains_accepted() {
        assert_eq!(
            plan("secret_parameter")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            2
        );
    }
    #[test]
    fn pending_secret_return_remains_accepted() {
        assert_eq!(
            plan("secret_return")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            2
        );
    }
    #[test]
    fn pending_secret_coalesce_remains_accepted() {
        assert_eq!(
            plan("secret_coalesce")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            3
        );
    }
    #[test]
    fn complete_local_absence_keeps_main_plan_acceptance() {
        assert_eq!(
            plan("complete_local_absence")
                .plan
                .effects()
                .iter()
                .filter(|effect| matches!(effect, Effect::Create(_)))
                .count(),
            2
        );
    }
}

mod selection_diagnostics {
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };
    fn run(dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(
                "CARINA_MOCK_STATE_FILE",
                dir.join("mock-provider-state.json"),
            )
            .env_remove("CARINA_KNOWLEDGE_MISSING_INPUT")
            .env(
                "CARINA_KNOWLEDGE_SELECTION_KEY",
                "PRIVATE_SELECTION_ENV_KEY",
            )
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .unwrap()
    }
    fn fixture(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/value_knowledge")
            .join(name);
        for name in ["providers.crn", "backend.crn"] {
            let path = source.join(name);
            if path.exists() {
                fs::copy(path, dir.path().join(name)).unwrap();
            }
        }
        let init = run(dir.path(), &["init", "."]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
        dir
    }
    #[test]
    fn complete_local_index_miss_keeps_the_main_plan() {
        let dir = fixture("complete_local_index_miss");
        let output = run(dir.path(), &["plan", "--out", "plan.json", "."]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: carina_cli::commands::plan::PlanFile =
            serde_json::from_str(&fs::read_to_string(dir.path().join("plan.json")).unwrap())
                .unwrap();
        assert_eq!(plan.plan.effects().len(), 2);
        assert!(String::from_utf8_lossy(&output.stdout).contains("role.names[3]"));
    }
    #[test]
    fn selection_failure_keeps_its_main_cause() {
        let dir = fixture("concrete_access_miss");
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("index 0 out of range for list of length 0")
        );
    }
    #[test]
    fn attempted_failure_keeps_its_main_cause() {
        let dir = fixture("unavailable");
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("invalid value inside secret(...)")
        );
    }
    #[test]
    fn missing_schema_keeps_its_main_cause() {
        let dir = fixture("missing_schema");
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Unknown resource type: mock.test.NoSuchResource")
        );
    }

    #[test]
    fn retained_selection_failure_keeps_its_main_cause() {
        let dir = fixture("retained_access_miss");
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("field 'absent' not found in map; available keys: <no keys>")
        );
    }
}

mod reference_resolution {
    use std::{
        fs,
        path::Path,
        process::{Command, Output},
    };
    fn run(dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(dir)
            .env("NO_COLOR", "1")
            .env(
                "CARINA_MOCK_STATE_FILE",
                dir.join("mock-provider-state.json"),
            )
            .env_remove("CARINA_4316_UNUSED_MISSING")
            .env_remove("SAVED_PLAN_DEFERRED_FOR_SECRET_3990")
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .unwrap()
    }
    fn fixture(name: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/value_knowledge")
            .join(name);
        for name in ["providers.crn", "backend.crn"] {
            let path = source.join(name);
            if path.exists() {
                fs::copy(path, dir.path().join(name)).unwrap();
            }
        }
        let init = run(dir.path(), &["init", "."]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            copy_fixture(&entry.path(), &dir.path().join(entry.file_name()));
        }
        dir
    }
    fn assert_plans(name: &str) {
        let dir = fixture(name);
        let output = run(dir.path(), &["plan", "--out", "plan.json", "."]);
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn transitive_name_reference_keeps_the_main_plan() {
        assert_plans("transitive_name");
    }
    #[test]
    fn transitive_map_reference_keeps_the_main_plan() {
        assert_plans("transitive_map");
    }
    #[test]
    fn unused_bad_cidr_preserves_main_pending_4323() {
        assert_main_rejection(
            "unused_bad_cidr",
            "cidr_subnet() invalid CIDR format: 'invalid' (expected 'address/prefix')",
        );
    }
    #[test]
    fn unused_function_env_preserves_main_pending_4323() {
        assert_main_rejection(
            "unused_function_env",
            "environment variable 'CARINA_4316_UNUSED_MISSING' is not set",
        );
    }
    #[test]
    fn unused_top_env_preserves_main_pending_4323() {
        assert_main_rejection(
            "unused_top_env",
            "env(): environment variable 'CARINA_4316_UNUSED_MISSING' is not set",
        );
    }
    #[test]
    fn unselected_env_preserves_main_pending_4323() {
        assert_plans("unselected_env");
    }
    #[test]
    fn unavailable_required_attribute_keeps_main_sibling_validation() {
        let dir = fixture("required_failure");
        let output = run(dir.path(), &["validate", "."]);
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        // Unavailable is classified as Failed, rather than a classifier Err. Main
        // defers that failure until plan; validation still checks the sibling.
        assert!(!text.contains("invalid value inside secret(...)"), "{text}");
        assert!(text.contains("expected Map"), "{text}");
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("Error:"))
                .count(),
            1,
            "{text}"
        );
        assert!(!text.contains("Missing required"), "{text}");
    }

    fn assert_main_rejection(name: &str, cause: &str) {
        let dir = fixture(name);
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(cause),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn copy_fixture(source: &Path, target: &Path) {
        if source.is_dir() {
            fs::create_dir_all(target).unwrap();
            for entry in fs::read_dir(source).unwrap() {
                let entry = entry.unwrap();
                copy_fixture(&entry.path(), &target.join(entry.file_name()));
            }
        } else {
            fs::copy(source, target).unwrap();
        }
    }
    #[test]
    fn failed_module_value_keeps_the_main_parse_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/saved_plan_deferred_for_secrets");
        copy_fixture(&source, dir.path());
        let output = run(dir.path(), &["plan", "."]);
        assert!(!output.status.success());
        let text = String::from_utf8_lossy(&output.stderr);
        assert!(text.starts_with("Error: Module resolution error: Parse error: nested-loops/main.crn:14:3: Invalid expression at line 16: invalid value inside secret(...)"), "{text}");
    }

    #[test]
    fn complete_updating_reference_keeps_the_main_plan() {
        let dir = fixture("updating_producer");
        let updated = fs::read_to_string(dir.path().join("main.crn")).unwrap();
        fs::copy(
            dir.path().join("baseline.crn.txt"),
            dir.path().join("main.crn"),
        )
        .unwrap();
        let output = run(
            dir.path(),
            &["apply", "--auto-approve", "--lock=false", "."],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::write(dir.path().join("main.crn"), updated).unwrap();
        let output = run(dir.path(), &["plan", "--refresh=false", "."]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("known after apply"));
    }

    #[test]
    fn empty_data_source_selection_keeps_the_main_plan() {
        for name in ["empty_read_indexed", "empty_read_chained"] {
            assert_plans(name);
        }
    }
}

mod retained_flatten_identity {
    use std::{fs, path::Path, process::Command};

    #[test]
    fn retained_flatten_reconciles_main_simhash_state_with_one_move() {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/value_knowledge/flatten_identity");
        for name in ["main.crn", "providers.crn", "exports.crn", "backend.crn"] {
            fs::copy(source.join(name), dir.path().join(name)).unwrap();
        }
        // Written by main's mock apply: flatten became List[ResourceRef] before
        // SimHash identity allocation. Applied attributes contain the resolved ID.
        fs::copy(
            source.join("main-written.state.json"),
            dir.path().join("carina.state.json"),
        )
        .unwrap();
        let run = |args: &[&str]| {
            Command::new(env!("CARGO_BIN_EXE_carina"))
                .current_dir(dir.path())
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .env(
                    "CARINA_MOCK_STATE_FILE",
                    dir.path().join("mock-provider-state.json"),
                )
                .args(args)
                .output()
                .unwrap()
        };
        let init = run(&["init", "."]);
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        let output = run(&["plan", "--refresh=false", "--out", "plan.json", "."]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let plan: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("plan.json")).unwrap()).unwrap();
        // Intended #4316 change: retaining flatten changes the anonymous identity,
        // and SimHash reconciliation moves the main-written instance, without
        // Create/Delete or a compatibility representation.
        assert_eq!(
            plan["plan"]["effects"],
            serde_json::json!([{"Move": {
                "from": {"provider":"mock", "resource_type":"example.consumer", "identity":"mock_example_consumer_c0440601002a1730"},
                "to": {"provider":"mock", "resource_type":"example.consumer", "identity":"mock_example_consumer_c064038420ad1600"}
            }}])
        );
        assert!(plan["unresolved_resources"][1]["attributes"]["list"]["FunctionCall"].is_object());
    }
}

mod typed_plan_errors {
    use carina_cli::{error::AppError, wiring::PlanPreparationError};
    use carina_core::resource::{Resource, knowledge::KnowledgeError};
    #[test]
    fn plan_knowledge_failure_reaches_a_typed_terminal_app_error() {
        let resource: Resource = Resource::with_provider("mock", "test.resource", "consumer", None);
        let preparation = PlanPreparationError::Knowledge {
            resource: Box::new(resource.id),
            attribute: "comment".into(),
            error: KnowledgeError::InvalidStage {
                operation: "known-value projection",
            },
        };
        let error: AppError = preparation.into();
        assert_eq!(
            error.to_string(),
            "mock.test.resource.consumer.comment: known-value projection is not valid at this resolution boundary"
        );
        assert!(std::error::Error::source(&error).is_none());
        assert!(
            matches!(error, AppError::PlanPreparation(error) if matches!(*error, PlanPreparationError::Knowledge { error: KnowledgeError::InvalidStage { .. }, .. }))
        );
    }
}
