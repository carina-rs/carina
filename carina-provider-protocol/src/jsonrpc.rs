//! JSON-RPC 2.0 message types for stdin/stdout communication.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// JSON-RPC request sent from host to provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(serde_json::to_value(params).unwrap_or(JsonValue::Null)),
        }
    }
}

/// JSON-RPC response sent from provider to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: u64, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(JsonValue::Null)),
            error: None,
        }
    }

    pub fn error(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

/// Notification sent from provider to host (no id, no response expected).
/// Used for the "ready" message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Notification {
    pub fn ready() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: "ready".into(),
            params: Some(serde_json::json!({
                "protocol_version": crate::PROTOCOL_VERSION,
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::new(1, "read", serde_json::json!({"id": "test"}));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"read\""));
    }

    #[test]
    fn test_response_success() {
        let resp = Response::success(1, serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_response_error() {
        let resp = Response::error(1, -1, "something failed");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
    }

    #[test]
    fn test_notification_ready() {
        let notif = Notification::ready();
        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains("\"method\":\"ready\""));
        assert!(!json.contains("\"id\""));
    }

    #[test]
    fn test_notification_ready_includes_protocol_version() {
        let notif = Notification::ready();
        let json: serde_json::Value = serde_json::to_value(&notif).unwrap();
        let params = json.get("params").expect("ready should have params");
        let version = params
            .get("protocol_version")
            .expect("params should have protocol_version");
        assert_eq!(version, &serde_json::Value::Number(1.into()));
    }
}
