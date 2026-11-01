use std::collections::HashMap;
use std::path::Path;

use crate::output_style::Colorize;

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::parser::ProviderContext;
use carina_core::value::{PersistedExportValue, render_export_json_value};
use carina_state::{StateUrl, load_published_exports_from_url};

use crate::display::colored_value;
use crate::error::AppError;
use carina_core::hint::ProjectCommand;

use super::{BackendLockRequirement, ReadBackendWarningPolicy, resolve_backend_for_read};

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
    color_mode: crate::output_style::ColorMode,
    path: &Path,
    state_url: Option<&str>,
    name: Option<String>,
    format: OutputFormat,
    provider_context: &ProviderContext,
) -> Result<(), AppError> {
    let exports = if let Some(raw_url) = state_url {
        let url = StateUrl::parse(raw_url).map_err(AppError::Backend)?;
        load_published_exports_from_url(&url)
            .await
            .map_err(AppError::Backend)?
    } else {
        let parsed = load_configuration_with_config(
            path,
            provider_context,
            &carina_core::schema::SchemaRegistry::new(),
        )?
        .parsed;
        let base_dir = get_base_dir(path);
        let backend_config = parsed
            .backend
            .clone()
            .map(carina_core::parser::BackendConfig::into_expanded)
            .transpose()?;
        let backend = resolve_backend_for_read(
            color_mode,
            base_dir,
            path,
            backend_config.as_ref(),
            BackendLockRequirement::AllowMissing,
            ReadBackendWarningPolicy::CommandBoundary { project_dir: path },
        )
        .await?;
        backend
            .read_published_exports()
            .await
            .map_err(AppError::Backend)?
            .ok_or_else(|| {
                AppError::Config(format!(
                    "No state file found. Run `{}` first.",
                    ProjectCommand::new("apply", path)
                ))
            })?
    };

    match name {
        Some(key) => print_single_export(color_mode, &key, exports.values(), &format),
        None => print_all_exports(color_mode, exports.values(), &format),
    }
}

fn print_single_export(
    color_mode: crate::output_style::ColorMode,
    key: &str,
    exports: &HashMap<String, PersistedExportValue>,
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
            println!("{}", format_raw_value(&value.decoded_json()));
        }
        OutputFormat::Json => {
            let value = value.decoded_json();
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
            );
        }
        OutputFormat::Human => {
            println!(
                "{} = {}",
                key.bold(color_mode),
                colored_value(color_mode, &render_export_json_value(value))
            );
        }
    }

    Ok(())
}

fn print_all_exports(
    color_mode: crate::output_style::ColorMode,
    exports: &HashMap<String, PersistedExportValue>,
    format: &OutputFormat,
) -> Result<(), AppError> {
    if exports.is_empty() {
        if matches!(format, OutputFormat::Json) {
            println!("{{}}");
        } else {
            println!("{}", "No exports defined.".dimmed(color_mode));
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
            let exports: std::collections::BTreeMap<_, _> = exports
                .iter()
                .map(|(key, value)| (key, value.decoded_json()))
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&exports).unwrap_or_else(|_| format!("{:?}", exports))
            );
        }
        OutputFormat::Human => {
            let mut keys: Vec<&String> = exports.keys().collect();
            keys.sort();
            for key in keys {
                println!(
                    "{} = {}",
                    key.bold(color_mode),
                    colored_value(color_mode, &render_export_json_value(&exports[key]))
                );
            }
        }
    }

    Ok(())
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

    fn persisted(value: serde_json::Value) -> PersistedExportValue {
        serde_json::from_value(value).expect("test value is valid persisted JSON")
    }

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
    fn human_json_value_string() {
        let v = persisted(serde_json::Value::String("vpc-0abc123".into()));
        assert_eq!(render_export_json_value(&v).plain(), "'vpc-0abc123'");
    }

    #[test]
    fn human_json_value_array() {
        let v = persisted(serde_json::json!(["459524413166", "151116838382"]));
        let formatted = render_export_json_value(&v);
        assert_eq!(formatted.plain(), "['459524413166', '151116838382']");
    }

    #[test]
    fn human_json_value_number() {
        let v = persisted(serde_json::json!(42));
        assert_eq!(render_export_json_value(&v).plain(), "42");
    }

    #[test]
    fn human_json_value_object_uses_carina_literals_recursively() {
        let value = persisted(serde_json::json!({
            "safe_key": "plain",
            "it's": ["quoted", {"a-b": "nested"}],
        }));

        assert_eq!(
            render_export_json_value(&value).plain(),
            r#"{"it's": ['quoted', {'a-b': 'nested'}], safe_key: 'plain'}"#
        );
    }

    #[test]
    fn print_single_export_found() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let mut exports = HashMap::new();
        exports.insert(
            "vpc_id".to_string(),
            persisted(serde_json::json!("vpc-0abc123")),
        );
        let result = print_single_export(color_mode, "vpc_id", &exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_single_export_not_found() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let mut exports = HashMap::new();
        exports.insert(
            "vpc_id".to_string(),
            persisted(serde_json::json!("vpc-0abc123")),
        );
        let err =
            print_single_export(color_mode, "missing", &exports, &OutputFormat::Human).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"));
        assert!(msg.contains("vpc_id"));
    }

    #[test]
    fn print_single_export_not_found_empty() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let exports = HashMap::new();
        let err =
            print_single_export(color_mode, "missing", &exports, &OutputFormat::Human).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("(none)"));
    }

    #[test]
    fn print_all_exports_empty_human() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let exports = HashMap::new();
        let result = print_all_exports(color_mode, &exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_empty_json() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let exports = HashMap::new();
        let result = print_all_exports(color_mode, &exports, &OutputFormat::Json);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_raw_requires_name() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let mut exports = HashMap::new();
        exports.insert("key".to_string(), persisted(serde_json::json!("value")));
        let err = print_all_exports(color_mode, &exports, &OutputFormat::Raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--raw requires"));
    }

    #[test]
    fn print_all_exports_human() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let mut exports = HashMap::new();
        exports.insert(
            "vpc_id".to_string(),
            persisted(serde_json::json!("vpc-0abc123")),
        );
        exports.insert(
            "accounts".to_string(),
            persisted(serde_json::json!(["459524413166"])),
        );
        let result = print_all_exports(color_mode, &exports, &OutputFormat::Human);
        assert!(result.is_ok());
    }

    #[test]
    fn print_all_exports_json() {
        let color_mode = crate::output_style::ColorMode::Plain;
        let mut exports = HashMap::new();
        exports.insert(
            "vpc_id".to_string(),
            persisted(serde_json::json!("vpc-0abc123")),
        );
        let result = print_all_exports(color_mode, &exports, &OutputFormat::Json);
        assert!(result.is_ok());
    }
}
