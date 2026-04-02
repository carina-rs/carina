//! Spawn provider binary as child process and communicate via JSON-RPC over stdin/stdout.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use carina_provider_protocol::jsonrpc::{Request, Response};

/// A running provider child process with JSON-RPC communication.
pub struct ProviderProcess {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    writer: BufWriter<std::process::ChildStdin>,
    next_id: AtomicU64,
}

fn validate_protocol_version(params: Option<&serde_json::Value>) -> Result<(), String> {
    let expected = carina_provider_protocol::PROTOCOL_VERSION;

    let params = params.ok_or_else(|| {
        format!(
            "Plugin did not report a protocol version. Carina requires protocol version {expected}."
        )
    })?;

    let version = params.get("protocol_version").and_then(|v| v.as_u64());

    match version {
        Some(v) if v as u32 == expected => Ok(()),
        Some(v) => {
            if (v as u32) < expected {
                Err(format!(
                    "Plugin uses protocol version {v}, but Carina requires version {expected}. \
                     Please update the plugin."
                ))
            } else {
                Err(format!(
                    "Plugin uses protocol version {v}, but this version of Carina only supports \
                     version {expected}. Please update Carina."
                ))
            }
        }
        None => Err(format!(
            "Plugin did not report a protocol version. Carina requires protocol version {expected}."
        )),
    }
}

impl ProviderProcess {
    /// Spawn a provider binary and wait for the "ready" notification.
    pub fn spawn(binary_path: &Path) -> Result<Self, String> {
        let mut child = Command::new(binary_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Forward stderr for logging
            .spawn()
            .map_err(|e| format!("Failed to spawn provider {}: {e}", binary_path.display()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to capture provider stdout")?;
        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to capture provider stdin")?;

        let mut reader = BufReader::new(stdout);
        let writer = BufWriter::new(stdin);

        // Wait for ready notification
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("Failed to read ready message: {e}"))?;

        let trimmed = line.trim();
        if !trimmed.contains("\"ready\"") {
            return Err(format!("Expected ready notification, got: {trimmed}"));
        }

        // Validate protocol version from ready notification params
        let notification: carina_provider_protocol::jsonrpc::Notification =
            serde_json::from_str(trimmed)
                .map_err(|e| format!("Failed to parse ready notification: {e}"))?;
        validate_protocol_version(notification.params.as_ref())?;

        Ok(Self {
            child,
            reader,
            writer,
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    pub fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);

        let req_json =
            serde_json::to_string(&request).map_err(|e| format!("Serialize failed: {e}"))?;

        writeln!(self.writer, "{req_json}").map_err(|e| format!("Write failed: {e}"))?;
        self.writer
            .flush()
            .map_err(|e| format!("Flush failed: {e}"))?;

        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .map_err(|e| format!("Read failed: {e}"))?;

        let response: Response =
            serde_json::from_str(line.trim()).map_err(|e| format!("Parse response failed: {e}"))?;

        if let Some(err) = response.error {
            return Err(format!("RPC error ({}): {}", err.code, err.message));
        }

        let result = response
            .result
            .ok_or_else(|| "Response has neither result nor error".to_string())?;

        serde_json::from_value(result).map_err(|e| format!("Deserialize result failed: {e}"))
    }

    /// Send shutdown and wait for process to exit.
    pub fn shutdown(&mut self) {
        let _ =
            self.call::<serde_json::Value, serde_json::Value>("shutdown", &serde_json::json!({}));
        let _ = self.child.wait();
    }
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_protocol_version_matching() {
        let params =
            serde_json::json!({ "protocol_version": carina_provider_protocol::PROTOCOL_VERSION });
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_protocol_version_mismatch() {
        let params = serde_json::json!({ "protocol_version": 999 });
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("protocol version 999"));
        assert!(err.contains(&format!(
            "version {}",
            carina_provider_protocol::PROTOCOL_VERSION
        )));
    }

    #[test]
    fn test_validate_protocol_version_missing_params() {
        let result = validate_protocol_version(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("did not report a protocol version"));
    }

    #[test]
    fn test_validate_protocol_version_missing_field() {
        let params = serde_json::json!({});
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_err());
    }
}
