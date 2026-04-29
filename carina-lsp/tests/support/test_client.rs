//! In-process LSP test client that talks JSON-RPC over a duplex pipe to a
//! `tower_lsp` server backed by `carina_lsp::Backend`.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower_lsp::{LspService, Server};

use carina_core::parser::ProviderContext;
use carina_lsp::Backend;

use super::byte_helpers::find_subsequence;

#[allow(dead_code)]
pub struct TestClient {
    writer: tokio::io::DuplexStream,
    reader: tokio::io::DuplexStream,
    buffer: Vec<u8>,
    next_id: i64,
}

#[allow(dead_code)]
impl TestClient {
    pub async fn new() -> Self {
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

    pub async fn initialize(&mut self) -> Value {
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

    pub async fn open_document(&mut self, uri: &str, text: &str) {
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

    pub async fn _request_completion(&mut self, uri: &str, line: u32, character: u32) -> Value {
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

    pub async fn _read_notification(&mut self, method: &str, timeout: Duration) -> Option<Value> {
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

    pub async fn shutdown(&mut self) {
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
