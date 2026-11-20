//! Option 4: module files use their standalone Live prefix authority.
use super::{Fixture, text};
use serde_json::{Value, json};
use std::{path::Path, sync::LazyLock};

const INPUTS: &str = include_str!("../fixtures/deferred_prefix/r10/inputs.json");
const DIRECTORIES: &str = include_str!("../fixtures/deferred_prefix/r10/module_directories.json");
const PLACEMENTS: &str =
    include_str!("../fixtures/deferred_prefix/r10/known_placement_disagreements.json");

fn fixture(case: &str) -> Fixture {
    let inputs: Value = serde_json::from_str(INPUTS).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for (name, source) in inputs[case].as_object().unwrap() {
        let file = directory.path().join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source.as_str().unwrap()).unwrap();
        if name.ends_with(".crn") {
            files.push(name.clone());
        }
    }
    Fixture {
        directory,
        files,
        subject: "mock.test.resource.<pending>".into(),
    }
}

fn module_files(fixture: &Fixture, directory: &str) -> Vec<String> {
    fixture
        .files
        .iter()
        .filter(|file| Path::new(file).parent() == Some(Path::new(directory)))
        .cloned()
        .collect()
}

fn message(text: &str) -> String {
    static CONTEXT: LazyLock<regex_lite::Regex> =
        LazyLock::new(|| regex_lite::Regex::new(r"^[\w.:-]+\.<pending>:\s*").unwrap());
    CONTEXT.replace(text, "").into_owned()
}

fn multiset(events: &Value) -> Vec<String> {
    let mut events = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            json!({"range": event["range"], "severity": event["severity"],
                "message": message(event["message"].as_str().unwrap())})
            .to_string()
        })
        .collect::<Vec<_>>();
    events.sort();
    events
}

pub(super) fn cli_file_inventory(fixture: &Fixture, report: &Value, file: &str) -> Value {
    let source = std::fs::read_to_string(fixture.root().join(file)).unwrap();
    let position = |position: &Value| {
        let line = position["line"].as_u64().unwrap() as usize - 1;
        let column = position["column"].as_u64().unwrap() as usize - 1;
        let character = source
            .lines()
            .nth(line)
            .unwrap()
            .chars()
            .take(column)
            .map(char::len_utf16)
            .sum::<usize>();
        json!({"line": line, "character": character})
    };
    let events = report
        .get("errors")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter(|event| {
            assert!(
                event["location"].is_object(),
                "unexpected unlocated error: {event}"
            );
            event["location"]["file"] == file
        })
        .map(|event| {
            let severity = match event["severity"].as_str().unwrap() {
                "error" => 1,
                "warning" => 2,
                severity => panic!("unexpected severity: {severity}"),
            };
            json!({"message": event["message"], "severity": severity, "range": {
                "start": position(&event["location"]["range"]["start"]),
                "end": position(&event["location"]["range"]["end"])
            }})
        })
        .collect::<Vec<_>>();
    json!(events)
}

async fn assert_standalone(case: &str) -> Fixture {
    let fixture = fixture(case);
    let directories: Value = serde_json::from_str(DIRECTORIES).unwrap();
    for directory in directories[case].as_array().unwrap() {
        let directory = directory.as_str().unwrap();
        let human = fixture.cli(&["validate", directory]);
        let output = fixture.cli(&["validate", directory, "--json"]);
        assert_eq!(human.status.code(), Some(1), "{case}");
        assert_eq!(output.status.code(), Some(1), "{case}");
        assert!(output.stderr.is_empty(), "{case}: {output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["errors"].as_array().unwrap().len(), 1, "{case}");
        let files = module_files(&fixture, directory);
        let inventory = fixture.editor_inventory_for_files(&files).await;
        println!(
            "R10_STANDALONE={case} {}",
            json!({"text": text(&human), "json": report, "lsp": inventory})
        );
        for file in files {
            assert_eq!(
                multiset(&inventory[&file]),
                multiset(&cli_file_inventory(&fixture, &report, &file)),
                "{case}: only {file} opened; standalone module {directory}"
            );
        }
    }
    fixture
}

macro_rules! standalone_cases {
    ($($case:ident),+ $(,)?) => {
        pub(super) const CASES: &[&str] = &[$(stringify!($case)),+];
        $(#[tokio::test]
        async fn $case() {
            assert_standalone(stringify!($case)).await;
        })+
    };
}

mod standalone {
    use super::*;
    standalone_cases!(
        m1a_empty,
        m1a2_conflict,
        m1b_secret,
        m1c_parse,
        m1c_upstream_missing_state,
        m1c_upstream_missing_source,
        m1c_upstream_missing_export,
        m2_two_roots,
        m2_ancestor_provider,
        m4_unimported_child,
        m4_bad_argument_type,
        m4_missing_argument,
        m1c_upstream_prefix_missing_export,
        m1c_upstream_prefix_missing_source,
        r7b_formal_bare,
        r7b_formal_alias,
        r7b_formal_interpolation,
        r7b_formal_secret,
        r7b_formal_several,
    );
}

async fn assert_placement(case: &str) {
    let fixture = fixture(case);
    let expected: Value = serde_json::from_str(PLACEMENTS).unwrap();
    let directories: Value = serde_json::from_str(DIRECTORIES).unwrap();
    let directory = directories[case][0].as_str().unwrap();
    let human = fixture.cli(&["validate", directory]);
    let output = fixture.cli(&["validate", directory, "--json"]);
    let inventory = fixture
        .editor_inventory_for_files(&module_files(&fixture, directory))
        .await;
    assert_eq!(
        json!({"validate": {"text": text(&human), "text_exit": human.status.code(),
            "json": serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            "json_exit": output.status.code(), "json_stderr": String::from_utf8_lossy(&output.stderr)},
            "lsp": inventory}),
        expected[case],
        "exact inherited placement discrepancy, including every module file: {case}"
    );
}

#[tokio::test]
async fn r10_provider_known_placement_disagreement_from_main() {
    assert_placement("m4_standalone_with_provider").await;
}

#[tokio::test]
async fn r10_backend_known_placement_disagreement_from_main() {
    assert_placement("m4_standalone_with_backend").await;
}

#[tokio::test]
async fn r10_literal_only_module_is_intrinsic_and_caller_is_clean() {
    let fixture = assert_standalone("r9_literal_only").await;
    let module = fixture
        .editor_inventory_for_files(&["child/main.crn".into()])
        .await;
    assert_eq!(
        module["child/main.crn"],
        json!([{
            "message": "'name_prefix' must be known at plan time to generate 'name', but it depends on 'prefix', which is not resolved when names are generated",
            "severity": 1, "range": {"start":{"line":0,"character":21},"end":{"line":0,"character":41}}
        }])
    );
    assert_eq!(
        fixture
            .editor_inventory_for_files(&["main.crn".into()])
            .await,
        json!({"main.crn": []})
    );
    let human = fixture.cli(&["validate", "."]);
    let output = fixture.cli(&["validate", ".", "--json"]);
    assert!(human.status.success(), "{}", text(&human));
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["status"], "ok");
    assert_eq!(report["resource_count"], 1);
    assert!(report.get("errors").is_none());
}

#[test]
fn r10_standalone_corpus_accounts_for_every_module_directory() {
    let inputs: Value = serde_json::from_str(INPUTS).unwrap();
    let directories: Value = serde_json::from_str(DIRECTORIES).unwrap();
    assert_eq!(inputs.as_object().unwrap().len(), 24);
    assert_eq!(
        inputs.as_object().unwrap().keys().collect::<Vec<_>>(),
        directories.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    assert_eq!(
        directories
            .as_object()
            .unwrap()
            .values()
            .map(|dirs| dirs.as_array().unwrap().len())
            .sum::<usize>(),
        22
    );
    let tested = standalone::CASES
        .iter()
        .copied()
        .chain([
            "m4_standalone_with_provider",
            "m4_standalone_with_backend",
            "r9_literal_only",
        ])
        .collect::<std::collections::BTreeSet<_>>();
    let declared = directories
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, directories)| !directories.as_array().unwrap().is_empty())
        .map(|(case, _)| case.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(tested, declared, "every module directory has a named test");
    assert_eq!(directories["m3_invalid_attribute"], json!([]));
    assert_eq!(directories["m3_undefined_binding"], json!([]));
}
