use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_lsp::{LspService, Server};

use carina_core::parser::ProviderContext;
use carina_lsp::Backend;

struct TestClient {
    writer: tokio::io::DuplexStream,
    reader: tokio::io::DuplexStream,
    buffer: Vec<u8>,
    next_id: i64,
}

impl TestClient {
    async fn new() -> Self {
        let (client_writer, server_reader) = tokio::io::duplex(1024 * 1024);
        let (server_writer, client_reader) = tokio::io::duplex(1024 * 1024);

        let (service, socket) = LspService::new(|client| {
            let provider_context = ProviderContext {
                decryptor: None,
                validators: HashMap::new(),
                custom_type_validator: None,
                schema_types: Default::default(),
            };
            Backend::new(client, provider_context, None)
        });

        tokio::spawn(async move {
            Server::new(server_reader, server_writer, socket)
                .serve(service)
                .await;
        });

        TestClient {
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

    async fn send_message(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.writer.write_all(header.as_bytes()).await.unwrap();
        self.writer.write_all(body.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn read_message(&mut self) -> Value {
        loop {
            if let Some(msg) = self.try_parse_message() {
                return msg;
            }
            let mut tmp = [0u8; 4096];
            let n = self.reader.read(&mut tmp).await.unwrap();
            assert!(n > 0, "Server closed the connection unexpectedly");
            self.buffer.extend_from_slice(&tmp[..n]);
        }
    }

    fn try_parse_message(&mut self) -> Option<Value> {
        let header_end = find_subsequence(&self.buffer, b"\r\n\r\n")?;
        let header_str = std::str::from_utf8(&self.buffer[..header_end]).ok()?;

        let content_length: usize = header_str.lines().find_map(|line| {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("Content-Length:") {
                val.trim().parse().ok()
            } else {
                None
            }
        })?;

        let body_start = header_end + 4; // skip \r\n\r\n
        let body_end = body_start + content_length;

        if self.buffer.len() < body_end {
            return None;
        }

        let body = &self.buffer[body_start..body_end];
        let msg: Value = serde_json::from_slice(body).ok()?;

        self.buffer = self.buffer[body_end..].to_vec();
        Some(msg)
    }

    async fn initialize(&mut self) -> Value {
        let id = self.next_id();
        let init_request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": null,
                "capabilities": {},
                "rootUri": null
            }
        });

        self.send_message(&init_request).await;

        // Read initialize response (skip any log messages)
        let response = self.read_response(id).await;

        // Send initialized notification
        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        self.send_message(&initialized).await;

        response
    }

    async fn read_response(&mut self, expected_id: i64) -> Value {
        loop {
            let msg = self.read_message().await;
            if msg.get("id").and_then(|v| v.as_i64()) == Some(expected_id) {
                return msg;
            }
            // Otherwise it's a notification (like window/logMessage), skip it
        }
    }

    async fn open_document(&mut self, uri: &str, text: &str) {
        let notification = json!({
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
        });
        self.send_message(&notification).await;
    }

    async fn _request_completion(&mut self, uri: &str, line: u32, character: u32) -> Value {
        let id = self.next_id();
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "textDocument/completion",
            "params": {
                "textDocument": {
                    "uri": uri
                },
                "position": {
                    "line": line,
                    "character": character
                }
            }
        });
        self.send_message(&request).await;
        self.read_response(id).await
    }

    async fn _read_notification(&mut self, method: &str, timeout: Duration) -> Option<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Try to parse from buffer first
            if let Some(msg) = self.try_parse_message() {
                if msg.get("method").and_then(|v| v.as_str()) == Some(method) {
                    return Some(msg);
                }
                // Not the notification we want, continue
                continue;
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }

            let mut tmp = [0u8; 4096];
            match tokio::time::timeout(remaining, self.reader.read(&mut tmp)).await {
                Ok(Ok(0)) => return None, // Connection closed
                Ok(Ok(n)) => {
                    self.buffer.extend_from_slice(&tmp[..n]);
                }
                Ok(Err(_)) => return None,
                Err(_) => return None, // Timeout
            }
        }
    }

    async fn shutdown(&mut self) {
        let id = self.next_id();
        let shutdown = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "shutdown",
            "params": null
        });
        self.send_message(&shutdown).await;
        let _ = self.read_response(id).await;

        let exit = json!({
            "jsonrpc": "2.0",
            "method": "exit",
            "params": null
        });
        self.send_message(&exit).await;
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[tokio::test]
async fn test_initialize_returns_completion_provider() {
    let mut client = TestClient::new().await;
    let response = client.initialize().await;

    let capabilities = &response["result"]["capabilities"];

    // Verify completionProvider is present
    assert!(
        capabilities.get("completionProvider").is_some(),
        "Server should advertise completionProvider"
    );

    // Verify trigger characters include "."
    let trigger_chars = capabilities["completionProvider"]["triggerCharacters"]
        .as_array()
        .expect("triggerCharacters should be an array");

    let has_dot = trigger_chars.iter().any(|v| v.as_str() == Some("."));
    assert!(has_dot, "Trigger characters should include '.'");

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_struct_field_completion() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_struct.crn";
    let text = r#"awscc.ec2.SecurityGroup {
    group_description = "test"
    security_group_ingress {

    }
}"#;

    client.open_document(uri, text).await;

    // Small delay to let the server process didOpen
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Request completion inside the security_group_ingress block (line 3)
    let response = client._request_completion(uri, 3, 8).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    // Verify struct field completions
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"ip_protocol"),
        "Should have ip_protocol completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"from_port"),
        "Should have from_port completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"to_port"),
        "Should have to_port completion. Got: {:?}",
        labels
    );

    // Verify they are FIELD kind (5 in LSP spec)
    for item in items {
        let label = item["label"].as_str().unwrap_or("");
        if label == "ip_protocol" || label == "from_port" || label == "to_port" {
            assert_eq!(
                item["kind"].as_u64(),
                Some(5), // CompletionItemKind::FIELD
                "{} should have FIELD kind",
                label
            );
        }
    }

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_diagnostics_for_unknown_struct_field() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_diag.crn";
    let text = r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.SecurityGroup {
    name = "test-sg"
    group_description = "Test security group"
    security_group_ingress {
        ip_protocol = "tcp"
        unknown_field = "bad"
    }
}"#;

    client.open_document(uri, text).await;

    // Read publishDiagnostics notification
    let notification = client
        ._read_notification("textDocument/publishDiagnostics", Duration::from_secs(5))
        .await
        .expect("Should receive publishDiagnostics notification");

    let diagnostics = notification["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    // Find the unknown_field diagnostic
    let has_unknown_field = diagnostics.iter().any(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown_field"))
    });

    assert!(
        has_unknown_field,
        "Should have diagnostic about unknown_field. Got: {:?}",
        diagnostics
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect::<Vec<_>>()
    );

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_resource_attribute_completion() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_attr.crn";
    let text = "aws.s3.Bucket {\n    \n}";

    client.open_document(uri, text).await;

    // Small delay to let the server process didOpen
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Request completion inside the block (line 1, after indentation)
    let response = client._request_completion(uri, 1, 4).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"bucket"),
        "Should have 'bucket' attribute completion. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"versioning_status"),
        "Should have 'versioning_status' attribute completion. Got: {:?}",
        labels
    );

    client.shutdown().await;
}

#[ignore = "requires provider schemas"]
#[tokio::test]
async fn test_diagnostics_for_exclusive_required_attrs() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = "file:///tmp/test_exclusive.crn";
    // vpc_gateway_attachment requires exactly one of internet_gateway_id or vpn_gateway_id,
    // but here neither is specified.
    let text = r#"provider awscc {
    region = awscc.Region.ap_northeast_1
}

awscc.ec2.vpc_gateway_attachment {
    vpc_id = "vpc-12345678"
}"#;

    client.open_document(uri, text).await;

    let notification = client
        ._read_notification("textDocument/publishDiagnostics", Duration::from_secs(5))
        .await
        .expect("Should receive publishDiagnostics notification");

    let diagnostics = notification["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics should be an array");

    let has_exclusive_error = diagnostics.iter().any(|d| {
        d["message"]
            .as_str()
            .is_some_and(|m| m.contains("Exactly one of"))
    });

    assert!(
        has_exclusive_error,
        "Should have diagnostic about exclusive required attrs. Got: {:?}",
        diagnostics
            .iter()
            .filter_map(|d| d["message"].as_str())
            .collect::<Vec<_>>()
    );

    client.shutdown().await;
}

/// Regression for #2196 (empty-candidate side).
///
/// End-to-end test: a leaf config dir (mirror of
/// `infra/aws/management/github-oidc/` — multi-file, no sibling module
/// dirs) asks for completion inside `use { source = '.|' }`. The LSP
/// must return the `./` and `../` navigation anchors so the user has
/// a starting point; prior to the fix this call returned an empty list.
#[tokio::test]
async fn test_use_source_path_completion_offers_anchors_in_leaf_dir() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    // Mirror the real infra shape: a leaf config dir with main/providers/backend,
    // no `modules/` subtree of its own.
    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("providers.crn"),
        "provider aws {\n  region = \"us-east-1\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        leaf.join("backend.crn"),
        "backend {\n  type = \"local\"\n}\n",
    )
    .unwrap();

    // Multi-line shape — mirrors what the user types. The opening quote
    // has no matching closing quote yet (this is the normal mid-typing
    // state); the context detector expects that. A closed `'...'` shape
    // is also handled elsewhere in the code path but isn't what we
    // exercise here.
    let main_text = "let shared = use {\n  source = '.";
    let main_path = leaf.join("main.crn");
    std::fs::write(&main_path, main_text).unwrap();

    let uri = format!("file://{}", main_path.display());
    client.open_document(&uri, main_text).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cursor at end of line 1 (after the `.`).
    let last_line = main_text.lines().next_back().unwrap();
    let character = last_line.chars().count() as u32;
    let response = client._request_completion(&uri, 1, character).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"./"),
        "Should offer './' anchor. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"../"),
        "Should offer '../' anchor. Got: {:?}",
        labels
    );

    client.shutdown().await;
}

/// End-to-end regression guard for #2196 (context-detection side): a full
/// LSP session where the editor opens a `.crn` with a multi-line
/// `use { source = '…' }` and requests completion at the cursor inside
/// `source`. Exercises the real JSON-RPC path, not only the internal
/// `CompletionProvider::complete`.
#[tokio::test]
async fn test_use_source_path_completion_multiline() {
    // Lay out a real workspace on disk so the LSP can walk it from the
    // opened document's directory.
    let tmp = tempfile::tempdir().unwrap();
    let modules_dir = tmp.path().join("modules");
    std::fs::create_dir_all(modules_dir.join("network")).unwrap();
    std::fs::create_dir_all(modules_dir.join("shared")).unwrap();

    let crn_path = tmp.path().join("main.crn");
    let text = "let network = use {\n  source = './modules/\n}";
    std::fs::write(&crn_path, text).unwrap();

    let mut client = TestClient::new().await;
    client.initialize().await;

    let uri = format!("file://{}", crn_path.display());
    client.open_document(&uri, text).await;

    // Small delay to let the server process didOpen.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Cursor at the end of the `source = './modules/` line (line 1).
    let second_line = text.lines().nth(1).unwrap();
    let character = second_line.chars().count() as u32;
    let response = client._request_completion(&uri, 1, character).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");

    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert!(
        labels.contains(&"network/"),
        "Should suggest 'network/' directory from './modules/'. Got: {:?}",
        labels
    );
    assert!(
        labels.contains(&"shared/"),
        "Should suggest 'shared/' directory from './modules/'. Got: {:?}",
        labels
    );

    client.shutdown().await;
}

/// Regression for #2200: attribute-name position inside `use { ... }` must
/// offer `source`. Multi-file directory fixture mirrors the real
/// `infra/aws/management/<leaf>/` shape per CLAUDE.md's directory-scoped
/// rule, so we're exercising the handler against the same shape users
/// actually edit.
#[tokio::test]
async fn test_use_block_offers_source_attribute() {
    let mut client = TestClient::new().await;
    client.initialize().await;

    let tmp = tempfile::tempdir().unwrap();
    let leaf = tmp.path().join("leaf");
    std::fs::create_dir_all(&leaf).unwrap();
    std::fs::write(
        leaf.join("providers.crn"),
        "provider aws {\n  region = \"us-east-1\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        leaf.join("backend.crn"),
        "backend {\n  type = \"local\"\n}\n",
    )
    .unwrap();

    // Multi-line mid-typing shape. Cursor on the empty line inside the block.
    let main_text = "let shared = use {\n\n}\n";
    let main_path = leaf.join("main.crn");
    std::fs::write(&main_path, main_text).unwrap();

    let uri = format!("file://{}", main_path.display());
    client.open_document(&uri, main_text).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Line 1, col 0 — inside the block, attribute-name position.
    let response = client._request_completion(&uri, 1, 0).await;

    let items = response["result"]
        .as_array()
        .expect("Completion result should be an array");
    let labels: Vec<&str> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();

    assert_eq!(
        labels,
        vec!["source"],
        "use block should offer exactly one attribute ('source'). Got: {:?}",
        labels
    );

    client.shutdown().await;
}
