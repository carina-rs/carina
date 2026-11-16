//! Native outputs and direct signatures for directory-scoped import fixtures.
//! Edit each fixture's expected values directly and review the diff.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use carina_core::provider::loaded_provider_snapshot_from_factories;
use carina_core::{EnvironmentSnapshot, parser::ProviderContext, paths::ResolvedPath};
use carina_provider_mock::{ConfiguredBuiltinMock, MockBehavior, MockScope};
use serde::Deserialize;
use serde_json::{Value, json};

#[path = "import_outcome_matrix/lsp.rs"]
mod lsp;

const COLUMNS: [&str; 14] = [
    "validate_human",
    "validate_json",
    "lint",
    "lsp_caller",
    "lsp_target",
    "hover_module",
    "hover_argument",
    "completion_top_level",
    "completion_parameter",
    "completion_parameter_empty",
    "completion_snippet",
    "completion_export_reference",
    "callsite_argument_checks",
    "module_reference_checks",
];

#[derive(Deserialize)]
struct Fixture {
    id: String,
    row: String,
    route: String,
    usage: String,
    caller: String,
    immediate_importer: String,
    directories: Vec<String>,
    target_paths: Vec<String>,
    unreadable_paths: Vec<String>,
    runtime_unreadable: BTreeMap<String, Unreadable>,
    // Only runtime-only unreadable inputs have inline text in the metadata.
    files: BTreeMap<String, String>,
    schema_readiness: String,
}

#[derive(Deserialize)]
struct ExpectedFixture {
    fixture: Fixture,
    columns: BTreeMap<String, Expected>,
    signatures: Value,
}

#[derive(Deserialize)]
struct Expected {
    classification: Classification,
    value: Value,
}

#[derive(Deserialize)]
struct MainFixture {
    revision: String,
    columns: BTreeMap<String, Value>,
}

// Keep this closed list in sync with the fixture README. Unknown labels must
// fail deserialization instead of silently bypassing the baseline assertion.
#[derive(Deserialize)]
enum Classification {
    #[serde(rename = "main")]
    Main,
    #[serde(rename = "main-4485")]
    Main4485,
    #[serde(rename = "changed:user-prohibitions")]
    UserProhibitions,
    #[serde(rename = "changed:4398-placement")]
    Placement,
    #[serde(rename = "changed:4398-importability")]
    Importability,
    #[serde(rename = "changed:4398-readable-prohibitions")]
    ReadableProhibitions,
    #[serde(rename = "changed:A")]
    EmptyInterface,
    #[serde(rename = "changed:C")]
    EmptyDirectory,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Unreadable {
    InvalidUtf8,
    Mode000,
}

struct Inputs {
    temp: tempfile::TempDir,
    files: BTreeMap<String, String>,
    disk: BTreeMap<String, Vec<u8>>,
    mode000: Vec<PathBuf>,
}

impl Drop for Inputs {
    fn drop(&mut self) {
        self.restore_permissions();
    }
}

fn collect_files(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(root, &path, files);
        } else if path == root.join("expected.json") || path == root.join("main.json") {
            continue;
        } else {
            assert_eq!(path.extension().unwrap(), "crn");
            files.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                fs::read_to_string(&path).unwrap(),
            );
        }
    }
}

impl Inputs {
    fn new(fixtures: &Path, fixture: &Fixture) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        collect_files(
            &fixtures.join(&fixture.id),
            &fixtures.join(&fixture.id),
            &mut files,
        );
        files.extend(fixture.files.clone());
        for directory in &fixture.directories {
            fs::create_dir_all(temp.path().join(directory)).unwrap();
        }
        let mut disk = BTreeMap::new();
        let mut mode000 = Vec::new();
        for (relative, text) in &files {
            let path = temp.path().join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let bytes = match fixture.runtime_unreadable.get(relative) {
                Some(Unreadable::InvalidUtf8) => vec![0xff],
                Some(Unreadable::Mode000) | None => text.as_bytes().to_vec(),
            };
            fs::write(&path, &bytes).unwrap();
            disk.insert(relative.clone(), bytes);
            if matches!(
                fixture.runtime_unreadable.get(relative),
                Some(Unreadable::Mode000)
            ) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                    mode000.push(path.clone());
                }
                #[cfg(not(unix))]
                panic!("mode-000 fixtures require an unprivileged Unix runner");
            }
            if fixture.runtime_unreadable.contains_key(relative) {
                assert!(
                    fs::read_to_string(&path).is_err(),
                    "{}: {relative} must really be unreadable; run unprivileged",
                    fixture.id
                );
            }
        }
        assert_eq!(
            fixture.unreadable_paths.iter().collect::<BTreeSet<_>>(),
            fixture.runtime_unreadable.keys().collect()
        );
        Self {
            temp,
            files,
            disk,
            mode000,
        }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }

    fn restore_permissions(&self) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in &self.mode000 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
    }

    fn assert_restored(&self) {
        for (relative, bytes) in &self.disk {
            let path = self.root().join(relative);
            if self.mode000.contains(&path) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0);
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                    assert_eq!(&fs::read(&path).unwrap(), bytes);
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                }
            } else {
                assert_eq!(&fs::read(path).unwrap(), bytes);
            }
        }
    }
}

fn cli(root: &Path, args: &[&str]) -> Value {
    let home = root.join("test-home");
    fs::create_dir_all(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(args)
        .current_dir(root.join("caller"))
        .env("HOME", &home)
        .env("CARINA_TEST_HOME", &home)
        .env("CARINA_TRUST_RECORD_DIR", home.join("trust"))
        .env("CARINA_PLUGIN_CACHE_DIR", home.join("plugins"))
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("CARINA_MOCK_STATE_FILE")
        .env_remove("CARINA_MOCK_LOG_FILE")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_CONFIG_FILE", "/dev/null")
        .env("AWS_SHARED_CREDENTIALS_FILE", "/dev/null")
        .output()
        .unwrap();
    json!({"exit": output.status.code().unwrap(), "stdout": String::from_utf8(output.stdout).unwrap(),
        "stderr": String::from_utf8(output.stderr).unwrap()})
}

fn overlay(fixture: &Fixture, files: &BTreeMap<String, String>, mode: &str) -> String {
    let original = &files[&fixture.immediate_importer];
    let prefix = original
        .split("let result = service")
        .next()
        .unwrap()
        .trim_end();
    let call = match mode {
        "ui" => "let result = service {\n  n = 1\n}\n\n",
        "empty" => "let result = service {\n  \n}\n",
        "argument" => "let result = service { n = 'wrong' unexpected = 1 }\n",
        "reference" => {
            "let supplied = mock.test.resource { name = 'supplied' }\nlet result = service { n = supplied.name }\nmock.test.resource { name = result.absent }\n"
        }
        "exports" => {
            "let result = service {\n}\nlet sink = use { source = '../completion-sink' }\nlet sink_call = sink {\n  text = \n}\n"
        }
        mode => panic!("unknown editor overlay {mode}"),
    };
    format!("{prefix}\n{call}")
}

fn caller(fixture: &Fixture, snapshot: &BTreeMap<String, Value>) -> Value {
    json!(
        snapshot
            .iter()
            .filter(|(name, _)| name.starts_with("caller/")
                || (fixture.route == "nested" && name.starts_with("bridge/")))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    )
}

fn target(fixture: &Fixture, snapshot: &BTreeMap<String, Value>) -> Value {
    json!(
        fixture
            .target_paths
            .iter()
            .filter_map(|name| snapshot
                .get(name)
                .map(|value| (name.clone(), value.clone())))
            .collect::<BTreeMap<_, _>>()
    )
}

fn normalize(value: &Value, root: &Path) -> Value {
    match value {
        Value::String(text) => json!(text.replace(root.to_str().unwrap(), "$ROOT")),
        Value::Array(values) => json!(
            values
                .iter()
                .map(|value| normalize(value, root))
                .collect::<Vec<_>>()
        ),
        Value::Object(values) => json!(
            values
                .iter()
                .map(|(key, value)| (
                    key.replace(root.to_str().unwrap(), "$ROOT"),
                    normalize(value, root)
                ))
                .collect::<BTreeMap<_, _>>()
        ),
        Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
    }
}

// Compare only the prohibition set. The ordinary absent-attribute finding is
// deliberately LSP-only, as on main; its separate issue is #4485.
fn assert_prohibition_parity(cli: &Value, diagnostics: &Value, fixture: &Fixture) {
    let report: Value = serde_json::from_str(cli["stdout"].as_str().unwrap()).unwrap();
    let errors = report
        .get("errors")
        .map(|errors| errors.as_array().unwrap().as_slice())
        .unwrap_or(&[]);
    let cli = errors
        .iter()
        .filter(|error| {
            error["message"]
                .as_str()
                .unwrap()
                .contains("not allowed inside modules")
        })
        .collect::<Vec<_>>();
    let lsp = diagnostics["caller/main.crn"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|diagnostic| {
            diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("not allowed inside modules")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cli.len(),
        lsp.len(),
        "{}: CLI/LSP prohibition count",
        fixture.id
    );
    for (cli, lsp) in cli.into_iter().zip(lsp) {
        assert!(
            cli["message"]
                .as_str()
                .unwrap()
                .contains(lsp["message"].as_str().unwrap()),
            "{}: prohibition order/text: {cli:?} / {lsp:?}",
            fixture.id
        );
        assert_eq!(
            cli["location"]["file"], "main.crn",
            "{}: prohibition owner",
            fixture.id
        );
        for side in ["start", "end"] {
            assert_eq!(
                cli["location"]["range"][side]["line"].as_u64().unwrap(),
                lsp["range"][side]["line"].as_u64().unwrap() + 1,
                "{}: prohibition line",
                fixture.id
            );
            assert_eq!(
                cli["location"]["range"][side]["column"].as_u64().unwrap(),
                lsp["range"][side]["character"].as_u64().unwrap() + 1,
                "{}: prohibition column",
                fixture.id
            );
        }
    }
}

fn assert_decisions(fixture: &Fixture, cells: &BTreeMap<String, Value>) {
    assert_prohibition_parity(
        &cells["validate_json"],
        &cells["lsp_caller"]["faithful_disk_error"],
        fixture,
    );
    assert_prohibition_parity(
        &cells["validate_json"],
        &cells["lsp_caller"]["all_siblings_open"],
        fixture,
    );
    let report: Value =
        serde_json::from_str(cells["validate_json"]["stdout"].as_str().unwrap()).unwrap();
    if !fixture.runtime_unreadable.is_empty()
        && let Some(errors) = report["errors"].as_array()
        && errors.iter().any(|error| {
            error["message"]
                .as_str()
                .unwrap()
                .contains("not allowed inside modules")
        })
    {
        let read_errors = errors
            .iter()
            .filter(|error| error["message"].as_str().unwrap().starts_with("IO error:"))
            .collect::<Vec<_>>();
        let lsp_errors = cells["lsp_caller"]["faithful_disk_error"]["caller/main.crn"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|diagnostic| {
                diagnostic["message"]
                    .as_str()
                    .unwrap()
                    .starts_with("IO error:")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            read_errors.len(),
            lsp_errors.len(),
            "{}: IO count",
            fixture.id
        );
        assert_eq!(
            cells["validate_human"]["stderr"]
                .as_str()
                .unwrap()
                .matches("IO error:")
                .count(),
            read_errors.len(),
            "{}: human IO count",
            fixture.id
        );
        assert_eq!(
            cells["lint"]["stderr"]
                .as_str()
                .unwrap()
                .matches("IO error:")
                .count(),
            read_errors.len(),
            "{}: lint IO count",
            fixture.id
        );
        for (error, diagnostic) in read_errors.into_iter().zip(lsp_errors) {
            assert_eq!(
                error["message"], diagnostic["message"],
                "{}: IO text",
                fixture.id
            );
            assert_eq!(error["location"]["file"], "main.crn");
            for side in ["start", "end"] {
                assert_eq!(
                    error["location"]["range"][side]["line"].as_u64().unwrap(),
                    diagnostic["range"][side]["line"].as_u64().unwrap() + 1,
                    "{}: IO line",
                    fixture.id
                );
                assert_eq!(
                    error["location"]["range"][side]["column"].as_u64().unwrap(),
                    diagnostic["range"][side]["character"].as_u64().unwrap() + 1,
                    "{}: IO column",
                    fixture.id
                );
            }
        }
    }
    for column in ["callsite_argument_checks", "module_reference_checks"] {
        assert_prohibition_parity(
            &cells[column]["json"],
            &cells[column]["lsp_caller"],
            fixture,
        );
    }
    if fixture.row == "empty_directory" {
        let report: Value =
            serde_json::from_str(cells["validate_json"]["stdout"].as_str().unwrap()).unwrap();
        assert_eq!(
            report["errors"][0]["message"],
            "No .crn files found in ../module"
        );
        if fixture.usage == "uncalled" {
            assert!(
                report["errors"][0]["location"].is_null(),
                "uncalled empty import has no use anchor: {report}"
            );
        }
    }
    if fixture.row == "arguments_prohibitions_three" || fixture.row == "arguments_prohibitions_five"
    {
        let items = cells["completion_export_reference"].as_array().unwrap();
        assert!(
            items.iter().any(|item| item["label"] == "result.value"),
            "{}: positive export-reference control",
            fixture.id
        );
    }
}

fn signatures(fixture: &Fixture, inputs: &Inputs) -> Value {
    let bootstrap = ProviderContext::new(EnvironmentSnapshot::empty());
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(vec![Box::new(ConfiguredBuiltinMock::new(
            MockBehavior::default(),
            MockScope::memory(),
        ))]),
        carina_core::schema::SchemaRegistry::new(),
        EnvironmentSnapshot::empty(),
    );
    assert_eq!(loaded.schema_registry().len(), 24);
    let mut results = BTreeMap::new();
    for (label, context) in [
        ("bootstrap", &bootstrap),
        ("loaded-mock", &**loaded.provider_context()),
    ] {
        for relative in
            BTreeSet::from([fixture.caller.as_str(), fixture.immediate_importer.as_str()])
        {
            let path = inputs.root().join(relative);
            let parsed =
                carina_core::parser::parse_file(&inputs.files[relative], &path, context).unwrap();
            let signatures = carina_core::module_resolver::load_resolved_module_signatures(
                &parsed,
                &ResolvedPath::new(path.parent().unwrap()).unwrap(),
                context,
            );
            let signatures = signatures.into_iter().map(|(alias, signature)| (alias, json!({
                "arguments": signature.arguments.iter().map(|argument| json!({"name": argument.name,
                    "type": argument.static_type.known().map(ToString::to_string), "default": argument.default.is_some()})).collect::<Vec<_>>(),
                "attributes": signature.attributes.iter().map(|(name, ty)| (name, ty.as_ref().and_then(|ty| ty.known()).map(ToString::to_string))).collect::<BTreeMap<_, _>>(),
                "requires": signature.requires.len(),
            }))).collect::<BTreeMap<_, _>>();
            results.insert(format!("{label}:{relative}"), signatures);
        }
    }
    json!(results)
}

async fn measure(fixture: &Fixture, inputs: &Inputs) -> BTreeMap<String, Value> {
    let root = inputs.root();
    let mut cells = BTreeMap::new();
    for (column, args) in [
        ("validate_human", vec!["validate", "."]),
        ("validate_json", vec!["validate", "--json", "."]),
        ("lint", vec!["lint", "."]),
    ] {
        cells.insert(column.to_owned(), cli(root, &args));
    }
    let mut client = lsp::Client::new(root, &fixture.schema_readiness).await;
    let mut opened = inputs
        .files
        .keys()
        .filter(|name| !fixture.runtime_unreadable.contains_key(*name))
        .collect::<Vec<_>>();
    opened.sort_by_key(|name| (*name == &fixture.caller, *name));
    for name in &opened {
        client.document(root, name, &inputs.files[*name]).await;
    }
    for name in &opened {
        client.document(root, name, &inputs.files[*name]).await;
    }
    let disk = client.snapshot(root);
    for name in &fixture.unreadable_paths {
        client.document(root, name, &inputs.files[name]).await;
    }
    for (name, text) in &inputs.files {
        client.document(root, name, text).await;
    }
    let all = client.snapshot(root);
    cells.insert("lsp_caller".into(), json!({"all_siblings_open": caller(fixture, &all), "faithful_disk_error": caller(fixture, &disk)}));
    cells.insert("lsp_target".into(), json!({"all_siblings_open": target(fixture, &all), "faithful_disk_error": target(fixture, &disk)}));
    let owner = &fixture.immediate_importer;
    let ui = overlay(fixture, &inputs.files, "ui");
    client.document(root, owner, &ui).await;
    let lines = ui.lines().collect::<Vec<_>>();
    let call_line = lines
        .iter()
        .position(|line| line.starts_with("let result = service"))
        .unwrap();
    for (column, method, line, character) in [
        ("hover_module", "hover", call_line, 14),
        ("hover_argument", "hover", call_line + 1, 2),
        ("completion_parameter", "completion", call_line + 1, 2),
        ("completion_top_level", "completion", lines.len() - 1, 0),
    ] {
        cells.insert(
            column.into(),
            client.feature(root, owner, method, line, character).await,
        );
    }
    let top = &cells["completion_top_level"];
    let items = top.get("items").unwrap_or(top).as_array().unwrap();
    cells.insert(
        "completion_snippet".into(),
        json!(
            items
                .iter()
                .filter(|item| item["label"] == "service"
                    || item["insertText"]
                        .as_str()
                        .is_some_and(|text| text.starts_with("service {")))
                .collect::<Vec<_>>()
        ),
    );
    client
        .document(root, owner, &overlay(fixture, &inputs.files, "empty"))
        .await;
    cells.insert(
        "completion_parameter_empty".into(),
        client
            .feature(root, owner, "completion", call_line + 1, 2)
            .await,
    );
    let export = overlay(fixture, &inputs.files, "exports");
    client.document(root, owner, &export).await;
    let export_line = export.lines().position(|line| line == "  text = ").unwrap();
    cells.insert(
        "completion_export_reference".into(),
        client
            .feature(root, owner, "completion", export_line, "  text = ".len())
            .await,
    );
    for (mode, column) in [
        ("argument", "callsite_argument_checks"),
        ("reference", "module_reference_checks"),
    ] {
        let text = overlay(fixture, &inputs.files, mode);
        fs::write(root.join(owner), &text).unwrap();
        client.document(root, owner, &text).await;
        if owner != &fixture.caller {
            client
                .document(root, &fixture.caller, &inputs.files[&fixture.caller])
                .await;
        }
        let diagnostics = caller(fixture, &client.snapshot(root));
        let human = cli(root, &["validate", "."]);
        let json = cli(root, &["validate", "--json", "."]);
        fs::write(root.join(owner), &inputs.disk[owner]).unwrap();
        cells.insert(
            column.into(),
            json!({"human": human, "json": json, "lsp_caller": diagnostics}),
        );
    }
    client.document(root, owner, &inputs.files[owner]).await;
    if owner != &fixture.caller {
        client
            .document(root, &fixture.caller, &inputs.files[&fixture.caller])
            .await;
    }
    assert_eq!(
        client.snapshot(root),
        all,
        "{}: editor overlays were not restored",
        fixture.id
    );
    client.finish().await;
    cells
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_outcome_matrix() {
    let fixtures_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import_outcome_matrix");
    let mut directories = fs::read_dir(&fixtures_path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    directories.sort();
    let columns = COLUMNS
        .iter()
        .map(|column| column.to_string())
        .collect::<BTreeSet<_>>();
    for directory in directories {
        let expected: ExpectedFixture =
            serde_json::from_slice(&fs::read(directory.join("expected.json")).unwrap()).unwrap();
        let fixture = &expected.fixture;
        let main: MainFixture =
            serde_json::from_slice(&fs::read(directory.join("main.json")).unwrap()).unwrap();
        assert_eq!(main.revision, "66063dd7f96561c274807d6943b5441783ee3d71");
        assert_eq!(
            main.columns.keys().cloned().collect::<BTreeSet<_>>(),
            columns
        );
        assert_eq!(
            expected.columns.keys().cloned().collect::<BTreeSet<_>>(),
            columns,
            "{}: expected columns",
            fixture.id
        );
        for column in COLUMNS {
            let id = format!("{}:{column}", fixture.id);
            let cell = &expected.columns[column];
            if matches!(
                cell.classification,
                Classification::Main | Classification::Main4485
            ) {
                assert_eq!(
                    cell.value, main.columns[column],
                    "{id}: main classification must equal stored genuine main"
                );
            }
        }
        if fixture
            .runtime_unreadable
            .values()
            .any(|kind| matches!(kind, Unreadable::Mode000))
            && !mode000_supported()
        {
            eprintln!(
                "SKIP {}: mode-000 fixtures require an unprivileged Unix runner; root can read mode 000",
                fixture.id
            );
            continue;
        }
        let inputs = Inputs::new(&fixtures_path, fixture);
        let cells = measure(fixture, &inputs).await;
        let actual_signatures = signatures(fixture, &inputs);
        let mut failures = Vec::new();
        for column in COLUMNS {
            let id = format!("{}:{column}", fixture.id);
            let actual = normalize(&cells[column], inputs.root());
            let value = &expected.columns[column].value;
            if actual != *value {
                failures.push(format!("{id}\nexpected: {value}\nactual: {actual}"));
            }
        }
        if actual_signatures != expected.signatures {
            failures.push(format!(
                "{}: direct signatures\nexpected: {}\nactual: {actual_signatures}",
                fixture.id, expected.signatures
            ));
        }
        inputs.assert_restored();
        if !failures.is_empty() {
            let retained = inputs.root().to_path_buf();
            inputs.restore_permissions();
            std::mem::forget(inputs);
            panic!("{}\nRetained: {}", failures.join("\n"), retained.display());
        }
        assert_decisions(fixture, &cells);
    }
}

fn mode000_supported() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no preconditions and does not change process state.
        unsafe { libc::geteuid() != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_negative_calls_republish_root_diagnostics() {
    let fixtures_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/import_outcome_matrix");
    let expected: ExpectedFixture = serde_json::from_slice(
        &fs::read(fixtures_path.join("normal--nested--called/expected.json")).unwrap(),
    )
    .unwrap();
    let inputs = Inputs::new(&fixtures_path, &expected.fixture);
    let cells = measure(&expected.fixture, &inputs).await;
    let diagnostics = &cells["callsite_argument_checks"]["lsp_caller"]["caller/main.crn"];
    assert!(
        diagnostics
            .to_string()
            .contains("expected Int, got string 'wrong'"),
        "{diagnostics}"
    );
    inputs.assert_restored();
}
