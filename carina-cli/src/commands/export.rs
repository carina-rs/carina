use std::collections::HashMap;
use std::path::PathBuf;

use colored::Colorize;

use carina_core::config_loader::load_configuration_with_config;
use carina_core::parser::ProviderContext;
use carina_state::{StateBackend, resolve_backend};

use crate::error::AppError;

/// Output format for the export command.
pub enum OutputFormat {
    /// Human-readable display
    Human,
    /// JSON output
    Json,
    /// Raw value (no key, no quotes — for shell scripting)
    Raw,
}

/// Run the `carina export` command.
pub async fn run_export(
    path: &PathBuf,
    name: Option<String>,
    format: OutputFormat,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let parsed = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?
    .parsed;

    let backend: Box<dyn StateBackend> = resolve_backend(parsed.backend.as_ref())
        .await
        .map_err(AppError::Backend)?;

    let state_file = backend
        .read_state()
        .await
        .map_err(AppError::Backend)?
        .ok_or_else(|| {
            AppError::Config("No state file found. Run 'carina apply' first.".to_string())
        })?;

    let exports = &state_file.exports;

    match name {
        Some(key) => print_single_export(&key, exports, &format),
        None => print_all_exports(exports, &format),
    }
}

fn print_single_export(
    key: &str,
    exports: &HashMap<String, serde_json::Value>,
    format: &OutputFormat,
) -> Result<(), AppError> {
    let value = exports.get(key).ok_or_else(|| {
        AppError::Config(format!(
            "Export '{}' not found. Available exports: {}",
            key,
            if exports.is_empty() {
                "(none)".to_string()
            } else {
                let mut keys: Vec<&str> = exports.keys().map(|k| k.as_str()).collect();
                keys.sort();
                keys.join(", ")
            }
        ))
    })?;

    match format {
        OutputFormat::Raw => {
            println!("{}", format_raw_value(value));
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
            );
        }
        OutputFormat::Human => {
            println!("{} = {}", key.bold(), format_json_value(value));
        }
    }

    Ok(())
}

fn print_all_exports(
    exports: &HashMap<String, serde_json::Value>,
    format: &OutputFormat,
) -> Result<(), AppError> {
    if exports.is_empty() {
        if matches!(format, OutputFormat::Json) {
            println!("{{}}");
        } else {
            println!("{}", "No exports defined.".dimmed());
        }
        return Ok(());
    }

    match format {
        OutputFormat::Raw => {
            return Err(AppError::Config(
                "--raw requires a specific export name.".to_string(),
            ));
        }
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(exports).unwrap_or_else(|_| format!("{:?}", exports))
            );
        }
        OutputFormat::Human => {
            let mut keys: Vec<&String> = exports.keys().collect();
            keys.sort();
            for key in keys {
                println!("{} = {}", key.bold(), format_json_value(&exports[key]));
            }
        }
    }

    Ok(())
}

/// Format a serde_json::Value for human-readable display.
fn format_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("\"{}\"", s),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(format_json_value).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(map) => {
            serde_json::to_string_pretty(value).unwrap_or_else(|_| format!("{:?}", map))
        }
        _ => value.to_string(),
    }
}

/// Format a value for --raw output (no quoting, no key).
fn format_raw_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_raw_string() {
        let v = serde_json::Value::String("vpc-0abc123".into());
        assert_eq!(format_raw_value(&v), "vpc-0abc123");
    }

    #[test]
    fn format_raw_number() {
        let v = serde_json::json!(42);
        assert_eq!(format_raw_value(&v), "42");
    }

    #[test]
    fn format_raw_bool() {
        let v = serde_json::json!(true);
        assert_eq!(format_raw_value(&v), "true");
    }

    #[test]
    fn format_raw_array_is_json() {
        let v = serde_json::json!(["a", "b"]);
        let raw = format_raw_value(&v);
        assert!(raw.contains("a"));
        assert!(raw.contains("b"));
    }

    #[test]
    fn format_json_value_string() {
        let v = serde_json::Value::String("vpc-0abc123".into());
        assert_eq!(format_json_value(&v), "\"vpc-0abc123\"");
    }

    #[test]
    fn format_json_value_array() {
        let v = serde_json::json!(["459524413166", "151116838382"]);
        let formatted = format_json_value(&v);
        assert_eq!(formatted, "[\"459524413166\", \"151116838382\"]");
    }

    #[test]
    fn format_json_value_number() {
        let v = serde_json::json!(42);
        assert_eq!(format_json_value(&v), "42");
    }

    #[test]
    fn print_single_export_found() {
        let mut exports = HashMap::new();
        exports.insert("vpc_id".to_string(), serde_json::json!("vpc-0abc123"));
        let result = print_single_export("vpc_id", &exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_single_export_not_found() {
        let mut exports = HashMap::new();
        exports.insert("vpc_id".to_string(), serde_json::json!("vpc-0abc123"));
        let err = print_single_export("missing", &exports, &OutputFormat::Human).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"));
        assert!(msg.contains("vpc_id"));
    }

    #[test]
    fn print_single_export_not_found_empty() {
        let exports = HashMap::new();
        let err = print_single_export("missing", &exports, &OutputFormat::Human).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("(none)"));
    }

    #[test]
    fn print_all_exports_empty_human() {
        let exports = HashMap::new();
        let result = print_all_exports(&exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_empty_json() {
        let exports = HashMap::new();
        let result = print_all_exports(&exports, &OutputFormat::Json);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_raw_requires_name() {
        let mut exports = HashMap::new();
        exports.insert("key".to_string(), serde_json::json!("value"));
        let err = print_all_exports(&exports, &OutputFormat::Raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--raw requires"));
    }

    #[test]
    fn print_all_exports_human() {
        let mut exports = HashMap::new();
        exports.insert("vpc_id".to_string(), serde_json::json!("vpc-0abc123"));
        exports.insert("accounts".to_string(), serde_json::json!(["459524413166"]));
        let result = print_all_exports(&exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_json() {
        let mut exports = HashMap::new();
        exports.insert("vpc_id".to_string(), serde_json::json!("vpc-0abc123"));
        let result = print_all_exports(&exports, &OutputFormat::Json);
        assert!(result.is_ok());
    }
}
