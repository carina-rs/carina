use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

use carina_core::parser::ProviderContext;
use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use carina_lsp::Backend;
use carina_lsp::backend::FactoryBuilder;
use indexmap::IndexMap;
use serde_json::{Value as JsonValue, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_lsp::{LspService, Server};

const INPUT: &str = r#"provider mock {}

let target = mock.test.resource {
  name = "target"
  rules = [{
    action = "allow"
  }]
}
"#;

const INPUT_WITHOUT_PROVIDER: &str = r#"let target = mock.test.resource {
  name = "target"
  rules = [{
    action = "allow"
  }]
}
"#;

const PROVIDER_WARNING: &str = "Warning: provider(s) unavailable; block-syntax conversion skipped";

struct FormattingFactory;

impl ProviderFactory for FormattingFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Mock"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        String::new()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("formatting does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        let rules = AttributeType::list(AttributeType::struct_(
            "Rule",
            vec![StructField::new("action", AttributeType::string())],
        ));
        vec![
            ResourceSchema::new("test.resource")
                .attribute(AttributeSchema::new("rules", rules).with_block_name("rule")),
        ]
    }
}

fn formatting_factory_builder() -> FactoryBuilder {
    Arc::new(|configs| {
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(FormattingFactory)];
        let fingerprint = configs
            .iter()
            .map(|(_, config)| (config.name.clone(), true))
            .collect();
        (factories, HashMap::new(), fingerprint)
    })
}

struct FormattingClient {
    writer: tokio::io::DuplexStream,
    reader: tokio::io::DuplexStream,
    buffer: Vec<u8>,
    next_id: i64,
}

impl FormattingClient {
    async fn new(factory_builder: FactoryBuilder) -> Self {
        let (client_writer, server_reader) = tokio::io::duplex(1024 * 1024);
        let (server_writer, client_reader) = tokio::io::duplex(1024 * 1024);
        let (service, socket) = LspService::new(move |client| {
            Backend::new(
                client,
                ProviderContext::default(),
                Some(Arc::clone(&factory_builder)),
            )
        });
        tokio::spawn(async move {
            Server::new(server_reader, server_writer, socket)
                .serve(service)
                .await;
        });
        Self {
            writer: client_writer,
            reader: client_reader,
            buffer: Vec::new(),
            next_id: 1,
        }
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn send(&mut self, message: &JsonValue) {
        let body = serde_json::to_string(message).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.writer.write_all(header.as_bytes()).await.unwrap();
        self.writer.write_all(body.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_message(&mut self) -> JsonValue {
        loop {
            if let Some(message) = self.try_parse_message() {
                return message;
            }
            let mut bytes = [0_u8; 4096];
            let count = self.reader.read(&mut bytes).await.unwrap();
            assert!(count > 0, "LSP server closed the connection");
            self.buffer.extend_from_slice(&bytes[..count]);
        }
    }

    fn try_parse_message(&mut self) -> Option<JsonValue> {
        let header_end = self
            .buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let header = std::str::from_utf8(&self.buffer[..header_end]).ok()?;
        let content_length = header.lines().find_map(|line| {
            line.strip_prefix("Content-Length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })?;
        let body_start = header_end + 4;
        let body_end = body_start + content_length;
        if self.buffer.len() < body_end {
            return None;
        }
        let message = serde_json::from_slice(&self.buffer[body_start..body_end]).ok()?;
        self.buffer.drain(..body_end);
        Some(message)
    }

    async fn read_non_request(&mut self) -> JsonValue {
        loop {
            let message = self.read_message().await;
            if message.get("method").is_some()
                && let Some(id) = message.get("id").cloned()
            {
                self.send(&json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": null
                }))
                .await;
                continue;
            }
            return message;
        }
    }

    async fn read_response(&mut self, expected_id: i64) -> JsonValue {
        loop {
            let message = self.read_non_request().await;
            if message.get("id").and_then(JsonValue::as_i64) == Some(expected_id) {
                return message;
            }
        }
    }

    async fn initialize(&mut self, root: &Path) {
        let root_uri = tower_lsp::lsp_types::Url::from_directory_path(root).unwrap();
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": root_uri
            }
        }))
        .await;
        let response = self.read_response(id).await;
        assert!(response.get("error").is_none(), "initialize: {response}");
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        }))
        .await;

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = self.read_non_request().await;
                if message.get("method").and_then(JsonValue::as_str) == Some("window/logMessage")
                    && message["params"]["message"]
                        .as_str()
                        .is_some_and(|text| text.contains("Loaded providers for"))
                {
                    break;
                }
            }
        })
        .await
        .expect("LSP schema load timed out");
    }

    async fn open_document(&mut self, uri: &str, text: &str) {
        self.send(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": "carina",
                    "version": 1,
                    "text": text
                }
            }
        }))
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let message = self.read_non_request().await;
                if message.get("method").and_then(JsonValue::as_str)
                    == Some("textDocument/publishDiagnostics")
                    && message["params"]["uri"].as_str() == Some(uri)
                {
                    break;
                }
            }
        })
        .await
        .expect("LSP didOpen diagnostics timed out");
    }

    async fn format_document(&mut self, uri: &str) -> String {
        let id = self.next_id();
        self.send(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/formatting",
            "params": {
                "textDocument": { "uri": uri },
                "options": { "tabSize": 2, "insertSpaces": true }
            }
        }))
        .await;
        let response = self.read_response(id).await;
        response["result"]
            .as_array()
            .and_then(|edits| edits.first())
            .and_then(|edit| edit["newText"].as_str())
            .expect("formatting response must contain one full-document edit")
            .to_string()
    }
}

fn run_fmt_with_args(project: &Path, mock_schema: bool, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .arg("fmt")
        .args(args);
    if mock_schema {
        command.env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1");
    } else {
        command.env_remove("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA");
    }
    command.output().expect("run carina fmt")
}

fn run_fmt(project: &Path, mock_schema: bool) -> Output {
    run_fmt_with_args(project, mock_schema, &["."])
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "carina fmt failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
async fn real_cli_and_lsp_schema_acquisition_produce_byte_identical_formatting() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    std::fs::write(&main, INPUT).unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(&main)
        .unwrap()
        .to_string();
    let mut client = FormattingClient::new(formatting_factory_builder()).await;
    client.initialize(project.path()).await;
    client.open_document(&uri, INPUT).await;
    let lsp_formatted = client.format_document(&uri).await;

    // This pins real CLI/LSP formatting parity, but not production WASM schema
    // acquisition: the environment flag injects the mock schema even with no
    // factories. The block_names_for_dir and wiring tests cover builder-to-name
    // collection and production factory loading as separate seams.
    let output = run_fmt(project.path(), true);

    assert_success(&output);
    let cli_formatted = std::fs::read_to_string(main).unwrap();
    assert!(
        cli_formatted.contains("  rule {\n"),
        "formatted:\n{cli_formatted}"
    );
    assert!(
        !cli_formatted.contains("rules = ["),
        "list literal was not converted:\n{cli_formatted}"
    );
    assert_eq!(lsp_formatted.as_bytes(), cli_formatted.as_bytes());
}

#[tokio::test]
async fn recursive_fmt_uses_each_files_directory_and_matches_lsp() {
    let project = tempfile::tempdir().unwrap();
    let directory_a = project.path().join("a");
    let directory_b = project.path().join("b");
    std::fs::create_dir_all(&directory_a).unwrap();
    std::fs::create_dir_all(&directory_b).unwrap();
    let main_a = directory_a.join("main.crn");
    let main_b = directory_b.join("main.crn");
    std::fs::write(&main_a, INPUT).unwrap();
    std::fs::write(&main_b, INPUT_WITHOUT_PROVIDER).unwrap();

    let uri_a = tower_lsp::lsp_types::Url::from_file_path(&main_a)
        .unwrap()
        .to_string();
    let mut client = FormattingClient::new(formatting_factory_builder()).await;
    client.initialize(project.path()).await;
    client.open_document(&uri_a, INPUT).await;
    let lsp_formatted_a = client.format_document(&uri_a).await;

    let output = run_fmt_with_args(project.path(), true, &[".", "--recursive"]);

    assert_success(&output);
    let cli_formatted_a = std::fs::read_to_string(main_a).unwrap();
    let cli_formatted_b = std::fs::read_to_string(main_b).unwrap();
    let plain_formatted_b = carina_core::formatter::format(
        INPUT_WITHOUT_PROVIDER,
        &carina_core::formatter::FormatConfig::default(),
    )
    .unwrap();
    assert!(cli_formatted_a.contains("  rule {\n"));
    assert_eq!(cli_formatted_a.as_bytes(), lsp_formatted_a.as_bytes());
    assert_eq!(cli_formatted_b.as_bytes(), plain_formatted_b.as_bytes());
    assert!(cli_formatted_b.contains("rules = ["));
}

#[tokio::test]
async fn recursive_cli_formats_imported_module_with_callers_lsp_schemas() {
    let project = tempfile::tempdir().unwrap();
    let root = project.path().canonicalize().unwrap();
    let caller = root.join("caller");
    let module = root.join("modules").join("m");
    std::fs::create_dir_all(&caller).unwrap();
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        caller.join("main.crn"),
        r#"provider mock {}

let imported = use {
  source = '../modules/m'
}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  name: String\n}\n",
    )
    .unwrap();
    let module_resource = module.join("resource.crn");
    std::fs::write(&module_resource, INPUT_WITHOUT_PROVIDER).unwrap();
    let uri = tower_lsp::lsp_types::Url::from_file_path(&module_resource)
        .unwrap()
        .to_string();
    let mut client = FormattingClient::new(formatting_factory_builder()).await;
    client.initialize(&root).await;
    client.open_document(&uri, INPUT_WITHOUT_PROVIDER).await;
    let lsp_formatted = client.format_document(&uri).await;

    let output = run_fmt_with_args(&root, true, &[".", "--recursive"]);

    assert_success(&output);
    let cli_formatted = std::fs::read_to_string(module_resource).unwrap();
    assert!(cli_formatted.contains("  rule {\n"), "{cli_formatted}");
    assert!(!cli_formatted.contains("rules = ["), "{cli_formatted}");
    assert_eq!(cli_formatted.as_bytes(), lsp_formatted.as_bytes());
}

#[test]
fn fmt_degrades_to_plain_formatting_when_provider_cannot_load() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    std::fs::write(
        &main,
        r#"provider broken {
  source = "https://unsupported.example/provider"
}

broken.test.resource {
  description="still format me"
}
"#,
    )
    .unwrap();

    let output = run_fmt(project.path(), false);

    assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "best-effort schema loading must not emit a hard provider error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let formatted = std::fs::read_to_string(main).unwrap();
    assert!(formatted.contains("description = 'still format me'"));
}

#[test]
fn check_notes_unavailable_providers_without_changing_exit_semantics() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    std::fs::write(
        &main,
        r#"provider broken {
  source = "https://unsupported.example/provider"
}

broken.test.resource {
  description="still format me"
}
"#,
    )
    .unwrap();

    let dirty_check = run_fmt_with_args(project.path(), false, &[".", "--check"]);

    assert!(!dirty_check.status.success());
    assert_eq!(
        String::from_utf8_lossy(&dirty_check.stderr)
            .matches(PROVIDER_WARNING)
            .count(),
        1
    );
    assert!(!String::from_utf8_lossy(&dirty_check.stdout).contains(PROVIDER_WARNING));

    let format = run_fmt(project.path(), false);
    assert_success(&format);
    assert!(format.stderr.is_empty());

    let clean_check = run_fmt_with_args(project.path(), false, &[".", "--check"]);

    assert!(
        clean_check.status.success(),
        "clean check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&clean_check.stdout),
        String::from_utf8_lossy(&clean_check.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&clean_check.stderr)
            .matches(PROVIDER_WARNING)
            .count(),
        1
    );
    assert!(!String::from_utf8_lossy(&clean_check.stdout).contains(PROVIDER_WARNING));
}

#[test]
fn check_surfaces_unversioned_lock_remediation() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("main.crn"),
        r#"provider awscc {
  source = 'github.com/carina-rs/carina-provider-awscc'
  revision = 'main'
}
"#,
    )
    .unwrap();
    std::fs::write(
        project.path().join("carina-providers.lock"),
        r#"[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
mode = "revision"
revision = "main"
resolved_sha = "967e645a7153522ca60ef942183d3fc338fc7c27"
sha256 = "3bd19254ba60717dabdc12c663ef96e0be72e5a2fbc192cf3a5d15ef6578f14f"
"#,
    )
    .unwrap();

    let format = run_fmt(project.path(), false);
    assert_success(&format);
    let output = run_fmt_with_args(project.path(), false, &[".", "--check"]);

    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(PROVIDER_WARNING), "{stderr}");
    assert!(
        stderr.contains("predates lock format versioning"),
        "{stderr}"
    );
    assert!(
        stderr.contains("restore this file from version control or backup"),
        "{stderr}"
    );
    assert!(
        stderr.contains("preserve it for manual inspection"),
        "{stderr}"
    );
}

#[test]
fn check_does_not_warn_for_sourceless_provider() {
    let project = tempfile::tempdir().unwrap();
    let main = project.path().join("main.crn");
    std::fs::write(&main, INPUT).unwrap();

    let format = run_fmt(project.path(), true);
    assert_success(&format);
    assert!(format.stderr.is_empty());
    let formatted = std::fs::read_to_string(main).unwrap();
    assert!(formatted.contains("  rule {\n"));
    assert!(!formatted.contains("rules = ["));

    let output = run_fmt_with_args(project.path(), true, &[".", "--check"]);

    assert_success(&output);
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(PROVIDER_WARNING),
        "sourceless providers do not represent a failed load: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(PROVIDER_WARNING));
}

#[test]
fn recursive_check_warns_once_for_multiple_unavailable_provider_directories() {
    let project = tempfile::tempdir().unwrap();
    for (directory, provider) in [("a", "broken_a"), ("b", "broken_b")] {
        let directory = project.path().join(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("main.crn"),
            format!(
                r#"provider {provider} {{
  source = "https://unsupported.example/provider"
}}
"#
            ),
        )
        .unwrap();
    }

    let format = run_fmt_with_args(project.path(), false, &[".", "--recursive"]);
    assert_success(&format);
    assert!(format.stderr.is_empty());

    let check = run_fmt_with_args(project.path(), false, &[".", "--recursive", "--check"]);

    assert_success(&check);
    assert_eq!(
        String::from_utf8_lossy(&check.stderr)
            .matches(PROVIDER_WARNING)
            .count(),
        1
    );
    assert!(!String::from_utf8_lossy(&check.stdout).contains(PROVIDER_WARNING));
}
