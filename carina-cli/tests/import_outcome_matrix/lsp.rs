//! Framed client for the production server, following fmt_block_names_e2e.
use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use carina_core::{EnvironmentSnapshot, parser::ProviderContext};
use carina_lsp::{Backend, backend::FactoryBuilder};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tower_lsp::{LspService, Server, lsp_types::Url};

pub struct Client {
    writer: DuplexStream,
    reader: DuplexStream,
    buffer: Vec<u8>,
    next_id: u64,
    versions: BTreeMap<String, u64>,
    pub diagnostics: BTreeMap<String, Value>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    pub async fn new(root: &Path, expected_readiness: &str) -> Self {
        let (writer, server_reader) = tokio::io::duplex(1024 * 1024);
        let (server_writer, reader) = tokio::io::duplex(1024 * 1024);
        // The production backend supplies its built-in mock when the builder
        // returns no external factory. This neither downloads nor mocks a schema.
        let builder: FactoryBuilder = Arc::new(|_| (Vec::new(), BTreeMap::new(), Vec::new()));
        let (service, socket) = LspService::new(move |client| {
            Backend::new(
                client,
                ProviderContext::new(EnvironmentSnapshot::empty()),
                Some(builder.clone()),
            )
        });
        let task = tokio::spawn(async move {
            Server::new(server_reader, server_writer, socket)
                .serve(service)
                .await;
        });
        let mut client = Self {
            writer,
            reader,
            buffer: Vec::new(),
            next_id: 0,
            versions: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            task,
        };
        client.request("initialize", json!({"processId": null,
            "rootUri": Url::from_directory_path(root).unwrap(),
            "capabilities": {"textDocument": {"completion": {"completionItem": {"snippetSupport": true}}}}
        })).await;
        client
            .send(json!({"method": "initialized", "params": {}}))
            .await;
        loop {
            let message = client.receive().await;
            if message["method"] == "window/logMessage"
                && let Some(text) = message["params"]["message"].as_str()
                && text.starts_with("Loaded providers for ")
            {
                assert_eq!(text, expected_readiness, "provider-scope readiness");
                break;
            }
        }
        client
    }

    async fn send(&mut self, mut message: Value) {
        message["jsonrpc"] = json!("2.0");
        let body = serde_json::to_vec(&message).unwrap();
        let mut packet = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        packet.extend(body);
        self.writer.write_all(&packet).await.unwrap();
        self.writer.flush().await.unwrap();
    }

    fn parse_message(&mut self) -> Option<Value> {
        let end = self
            .buffer
            .windows(4)
            .position(|part| part == b"\r\n\r\n")?;
        let header = std::str::from_utf8(&self.buffer[..end]).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let start = end + 4;
        if self.buffer.len() < start + length {
            return None;
        }
        let message = serde_json::from_slice(&self.buffer[start..start + length]).unwrap();
        self.buffer.drain(..start + length);
        Some(message)
    }

    async fn receive(&mut self) -> Value {
        // wall-clock-ok: hang guard for the production framed server.
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let message = loop {
                    if let Some(message) = self.parse_message() {
                        break message;
                    }
                    let mut bytes = [0_u8; 8192];
                    let count = self.reader.read(&mut bytes).await.unwrap();
                    assert_ne!(count, 0, "LSP closed before a response");
                    self.buffer.extend_from_slice(&bytes[..count]);
                };
                if message.get("method").is_some()
                    && let Some(id) = message.get("id")
                {
                    self.send(json!({"id": id, "result": null})).await;
                    continue;
                }
                if message["method"] == "textDocument/publishDiagnostics" {
                    self.diagnostics.insert(
                        message["params"]["uri"].as_str().unwrap().to_owned(),
                        message["params"]["diagnostics"].clone(),
                    );
                }
                return message;
            }
        })
        .await
        .expect("LSP response timed out")
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let message = if method == "shutdown" {
            json!({"id": id, "method": method})
        } else {
            json!({"id": id, "method": method, "params": params})
        };
        self.send(message).await;
        loop {
            let message = self.receive().await;
            if message["id"] == id {
                assert!(message.get("error").is_none(), "{message}");
                return message["result"].clone();
            }
        }
    }

    pub async fn document(&mut self, root: &Path, relative: &str, text: &str) {
        let uri = Url::from_file_path(root.join(relative))
            .unwrap()
            .to_string();
        if let Some(version) = self.versions.get_mut(relative) {
            *version += 1;
            let version = *version;
            self.send(json!({"method": "textDocument/didChange", "params": {
                "textDocument": {"uri": uri, "version": version}, "contentChanges": [{"text": text}]
            }}))
            .await;
        } else {
            self.versions.insert(relative.to_owned(), 1);
            self.send(
                json!({"method": "textDocument/didOpen", "params": {"textDocument": {
                    "uri": uri, "version": 1, "languageId": "carina", "text": text
                }}}),
            )
            .await;
        }
        loop {
            let message = self.receive().await;
            if message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri
            {
                break;
            }
        }
    }

    pub fn snapshot(&self, root: &Path) -> BTreeMap<String, Value> {
        self.diagnostics
            .iter()
            .map(|(uri, diagnostics)| {
                let path = Url::parse(uri).unwrap().to_file_path().unwrap();
                (
                    path.strip_prefix(root)
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                    diagnostics.clone(),
                )
            })
            .collect()
    }

    pub async fn feature(
        &mut self,
        root: &Path,
        relative: &str,
        method: &str,
        line: usize,
        character: usize,
    ) -> Value {
        let mut params = json!({"textDocument": {"uri": Url::from_file_path(root.join(relative)).unwrap()},
            "position": {"line": line, "character": character}});
        if method == "completion" {
            params["context"] = json!({"triggerKind": 1});
        }
        self.request(&format!("textDocument/{method}"), params)
            .await
    }

    pub async fn finish(&mut self) {
        assert_eq!(self.request("shutdown", Value::Null).await, Value::Null);
        self.send(json!({"method": "exit"})).await;
        self.writer.shutdown().await.unwrap();
        // wall-clock-ok: server shutdown hang guard.
        tokio::time::timeout(Duration::from_secs(15), &mut self.task)
            .await
            .unwrap()
            .unwrap();
    }
}
