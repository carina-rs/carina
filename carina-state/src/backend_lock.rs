//! Local backend-configuration lock file for change detection.
//!
//! Carina stores a snapshot of the current `backend` block in a local
//! file (`carina-backend.lock`) at the project root. Commands compare
//! the stored snapshot against the current configuration: `init` and
//! project-backed readers warn and keep reading the locked backend on
//! mismatch, while mutating commands refuse until `carina init
//! --migrate-state` explicitly moves state.
//!
//! This prevents silently pointing state operations at a different state
//! file, which could lead to resources being treated as unmanaged or
//! data loss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use carina_core::hint::ProjectCommand;
use serde::{Deserialize, Serialize};

use crate::backend::{BackendError, BackendResult, LOCAL_BACKEND_TYPE, S3_BACKEND_TYPE};
use crate::backend_config::BackendConfig;

/// Name of the lock file at the project root.
pub const LOCK_FILE: &str = "carina-backend.lock";

/// Legacy lock path components (for migration from `.carina/backend-lock.json`).
const LEGACY_LOCK_DIR: &str = ".carina";
const LEGACY_LOCK_FILE: &str = "backend-lock.json";

/// Snapshot of the backend configuration that was last used for a given
/// configuration root. Persisted to disk as JSON.
///
/// Attribute values are stored as strings and bools after typed backend
/// resolution. Keys are sorted in a `BTreeMap` so the serialized form is
/// deterministic regardless of parser-map iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLock {
    /// Backend type identifier (e.g., `"s3"`, `"local"`).
    backend_type: String,
    /// Sorted snapshot of backend attributes for stable comparison.
    attributes: BTreeMap<String, serde_json::Value>,
}

impl BackendLock {
    /// Build a lock snapshot directly from a typed backend configuration.
    /// Enum values use canonical API spelling, and optional attributes are
    /// included only when explicitly configured.
    pub fn from_config(config: &BackendConfig) -> Self {
        let (backend_type, attributes) = match config {
            BackendConfig::S3(settings) => {
                let mut attributes = BTreeMap::from([
                    (
                        "bucket".to_string(),
                        serde_json::Value::String(settings.bucket().to_string()),
                    ),
                    (
                        "key".to_string(),
                        serde_json::Value::String(settings.key().to_string()),
                    ),
                ]);
                if let Some(region) = settings.region() {
                    attributes.insert(
                        "region".to_string(),
                        serde_json::Value::String(region.to_string()),
                    );
                }
                if let Some(encrypt) = settings.authored_encrypt() {
                    attributes.insert("encrypt".to_string(), serde_json::Value::Bool(encrypt));
                }
                if let Some(auto_create) = settings.authored_auto_create() {
                    attributes.insert(
                        "auto_create".to_string(),
                        serde_json::Value::Bool(auto_create),
                    );
                }
                (S3_BACKEND_TYPE, attributes)
            }
            BackendConfig::Local(settings) => {
                let attributes = settings
                    .path()
                    .map(|path| {
                        BTreeMap::from([(
                            "path".to_string(),
                            serde_json::Value::String(path.to_string()),
                        )])
                    })
                    .unwrap_or_default();
                (LOCAL_BACKEND_TYPE, attributes)
            }
        };
        Self {
            backend_type: backend_type.to_string(),
            attributes,
        }
    }

    /// Build a lock snapshot representing the implicit local backend that
    /// is used when no `backend` block is configured. Allows drift
    /// inspection to detect local → remote transitions.
    pub fn local_default() -> Self {
        Self {
            backend_type: LOCAL_BACKEND_TYPE.to_string(),
            attributes: BTreeMap::new(),
        }
    }

    /// Lock snapshot for an optional parser backend block: the configured
    /// backend when present, the implicit local default when absent.
    ///
    /// This is the single source of truth for "what backend does this
    /// configuration name"; lock creation, drift inspection, `init`'s
    /// warning path, and the migration path all go through it so they
    /// cannot disagree.
    pub fn for_config(
        backend_config: Option<&carina_core::parser::BackendConfig>,
    ) -> BackendResult<Self> {
        match backend_config {
            Some(cfg) => BackendConfig::try_from(cfg)
                .map(|config| Self::from_config(&config))
                .map_err(BackendError::from),
            None => Ok(Self::local_default()),
        }
    }

    /// Path to the lock file under `base_dir` (project root).
    pub fn lock_path(base_dir: &Path) -> PathBuf {
        base_dir.join(LOCK_FILE)
    }

    /// Legacy lock path (`.carina/backend-lock.json`).
    fn legacy_lock_path(base_dir: &Path) -> PathBuf {
        base_dir.join(LEGACY_LOCK_DIR).join(LEGACY_LOCK_FILE)
    }

    /// Load an existing lock from `base_dir`, returning `None` if no
    /// lock file exists yet. Falls back to the legacy path for migration.
    pub fn load(base_dir: &Path) -> BackendResult<Option<Self>> {
        let path = Self::lock_path(base_dir);
        if path.exists() {
            return Self::load_from(&path, base_dir);
        }
        // Migration: try legacy path
        let legacy = Self::legacy_lock_path(base_dir);
        if legacy.exists() {
            return Self::load_from(&legacy, base_dir);
        }
        Ok(None)
    }

    fn load_from(path: &Path, project_dir: &Path) -> BackendResult<Option<Self>> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| BackendError::Io(format!("Failed to read {}: {}", path.display(), e)))?;
        let lock: Self = serde_json::from_str(&contents).map_err(|e| {
            BackendError::Serialization(format!(
                "Failed to parse backend lock at {}: {}",
                path.display(),
                e
            ))
        })?;
        // A syntactically valid JSON lock must still describe a configuration
        // accepted by the current typed backend schema. Validate eagerly so a
        // corrupt or obsolete address cannot participate in drift comparison.
        let config = lock
            .to_state_config()
            .map_err(|error| unreadable_backend_address(path, project_dir, error))?;
        Ok(Some(Self::from_config(&config)))
    }

    /// Persist this lock snapshot at the project root.
    pub fn save(&self, base_dir: &Path) -> BackendResult<()> {
        let path = Self::lock_path(base_dir);
        let contents = carina_core::utils::pretty_with_newline(self)
            .map_err(|e| BackendError::Serialization(e.to_string()))?;
        std::fs::write(&path, contents)
            .map_err(|e| BackendError::Io(format!("Failed to write {}: {}", path.display(), e)))?;
        Ok(())
    }

    /// Reconstruct a runtime [`BackendConfig`] from this locked snapshot.
    ///
    /// It is used by `init --migrate-state` to open the *old* backend (the
    /// address recorded in `carina-backend.lock`). JSON strings and bools are
    /// fed back through the same resolver as authored DSL. Any other JSON
    /// shape, unknown attribute, wrong type, or missing required attribute is
    /// an error rather than a guessed placeholder.
    ///
    /// [`from_config`]: BackendLock::from_config
    pub fn to_state_config(&self) -> BackendResult<BackendConfig> {
        use carina_core::resource::{ConcreteValue, Value};
        let attributes = self
            .attributes
            .iter()
            .map(|(key, value)| {
                let value: Value = match value {
                    serde_json::Value::String(s) => {
                        Value::Concrete(ConcreteValue::String(s.clone().into()))
                    }
                    serde_json::Value::Bool(b) => Value::Concrete(ConcreteValue::Bool(*b)),
                    other => {
                        return Err(BackendError::Serialization(format!(
                            "Invalid backend lock attribute '{key}': expected a string or bool, got {other}"
                        )));
                    }
                };
                Ok((key.clone(), value))
            })
            .collect::<BackendResult<_>>()?;
        let raw = carina_core::parser::BackendConfig {
            backend_type: self.backend_type.clone(),
            attributes,
            attribute_spans: Default::default(),
        };
        BackendConfig::try_from(&raw).map_err(BackendError::from)
    }

    /// Produce a human-readable diff description for error messages.
    pub fn describe_diff(&self, other: &Self) -> String {
        let mut lines = Vec::new();
        if self.backend_type != other.backend_type {
            lines.push(format!(
                "  backend type: {} → {}",
                self.backend_type, other.backend_type
            ));
        }
        let all_keys: std::collections::BTreeSet<&String> = self
            .attributes
            .keys()
            .chain(other.attributes.keys())
            .collect();
        for key in all_keys {
            let old_val = self.attributes.get(key);
            let new_val = other.attributes.get(key);
            if old_val != new_val {
                lines.push(format!(
                    "  {}: {} → {}",
                    key,
                    old_val.map_or("(unset)".to_string(), ToString::to_string),
                    new_val.map_or("(unset)".to_string(), ToString::to_string),
                ));
            }
        }
        lines.join("\n")
    }
}

fn unreadable_backend_address(
    path: &Path,
    project_dir: &Path,
    error: BackendError,
) -> BackendError {
    let detail = error.to_string();
    let init_command = ProjectCommand::new("init", project_dir);
    let message = format!(
        "Backend lock at {} records a backend address this release cannot read: {detail}. \
         To recover, verify the backend block, then delete `{}` and run `{init_command}` \
         to record it again.",
        path.display(),
        path.display(),
    );
    BackendError::UnreadableBackendAddress {
        message,
        error: Box::new(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::{ConcreteValue, Value};
    use std::collections::HashMap;

    fn resolve(backend_type: &str, attributes: HashMap<String, Value>) -> BackendConfig {
        let raw = carina_core::parser::BackendConfig {
            backend_type: backend_type.to_string(),
            attributes,
            attribute_spans: HashMap::new(),
        };
        BackendConfig::try_from(&raw).unwrap()
    }

    fn make_config(bucket: &str, region: &str) -> BackendConfig {
        resolve(
            S3_BACKEND_TYPE,
            HashMap::from([
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String(bucket.to_string().into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String(region.to_string().into())),
                ),
            ]),
        )
    }

    #[test]
    fn from_config_captures_type_and_attributes() {
        let config = make_config("my-bucket", "us-east-1");
        let lock = BackendLock::from_config(&config);
        assert_eq!(lock.backend_type, S3_BACKEND_TYPE);
        assert_eq!(
            lock.attributes.get("bucket"),
            Some(&serde_json::Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn lock_roundtrip_via_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = BackendLock::from_config(&make_config("b", "us-east-1"));
        lock.save(tmp.path()).unwrap();
        let loaded = BackendLock::load(tmp.path()).unwrap().unwrap();
        assert_eq!(lock, loaded);
    }

    #[test]
    fn load_returns_none_when_lock_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(BackendLock::load(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn lock_saves_to_project_root() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = BackendLock::from_config(&make_config("b", "us-east-1"));
        lock.save(tmp.path()).unwrap();
        // Should be at root, not in .carina/
        assert!(tmp.path().join("carina-backend.lock").exists());
        assert!(!tmp.path().join(".carina/backend-lock.json").exists());
    }

    // Pin the byte-level shape so carina-backend.lock and
    // carina-providers.lock stay consistent (both end with `\n`).
    #[test]
    fn lock_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = BackendLock::from_config(&make_config("b", "us-east-1"));
        lock.save(tmp.path()).unwrap();
        let bytes = std::fs::read(tmp.path().join("carina-backend.lock")).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "carina-backend.lock must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[test]
    fn load_migrates_from_legacy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let lock = BackendLock::from_config(&make_config("b", "us-east-1"));
        // Write to legacy path
        let legacy_dir = tmp.path().join(".carina");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        let contents = serde_json::to_string_pretty(&lock).unwrap();
        std::fs::write(legacy_dir.join("backend-lock.json"), contents).unwrap();
        // Load should find it
        let loaded = BackendLock::load(tmp.path()).unwrap().unwrap();
        assert_eq!(lock, loaded);
    }

    #[test]
    fn new_path_takes_precedence_over_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let old_lock = BackendLock::from_config(&make_config("old-bucket", "us-east-1"));
        let new_lock = BackendLock::from_config(&make_config("new-bucket", "us-east-1"));
        // Write old to legacy, new to root
        let legacy_dir = tmp.path().join(".carina");
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join("backend-lock.json"),
            serde_json::to_string_pretty(&old_lock).unwrap(),
        )
        .unwrap();
        new_lock.save(tmp.path()).unwrap();
        // Should load the new one
        let loaded = BackendLock::load(tmp.path()).unwrap().unwrap();
        assert_eq!(loaded, new_lock);
    }

    #[test]
    fn detects_bucket_change() {
        let a = BackendLock::from_config(&make_config("old-bucket", "us-east-1"));
        let b = BackendLock::from_config(&make_config("new-bucket", "us-east-1"));
        assert_ne!(a, b);
        let diff = a.describe_diff(&b);
        assert!(diff.contains("bucket"));
        assert!(diff.contains("old-bucket"));
        assert!(diff.contains("new-bucket"));
    }

    #[test]
    fn equal_configs_do_not_differ() {
        let a = BackendLock::from_config(&make_config("b", "us-east-1"));
        let b = BackendLock::from_config(&make_config("b", "us-east-1"));
        assert_eq!(a, b);
    }

    #[test]
    fn to_state_config_roundtrips_remote_attributes() {
        // A locked remote backend must be reconstructible into a
        // `StateBackendConfig` so the migration path can read the OLD
        // state from the address recorded in the lock.
        let original = make_config("old-bucket", "us-east-1");
        let lock = BackendLock::from_config(&original);
        let cfg = lock.to_state_config().unwrap();
        assert_eq!(cfg, original);
    }

    #[test]
    fn enum_region_is_locked_in_api_spelling_and_roundtrips() {
        let config = resolve(
            S3_BACKEND_TYPE,
            HashMap::from([
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::enum_identifier("aws.Region.us_west_2")),
                ),
            ]),
        );
        let lock = BackendLock::from_config(&config);
        assert_eq!(
            lock.attributes.get("region"),
            Some(&serde_json::Value::String("us-west-2".to_string()))
        );
        assert_eq!(lock.to_state_config().unwrap(), config);
    }

    #[test]
    fn to_state_config_of_local_default_is_local() {
        let cfg = BackendLock::local_default().to_state_config().unwrap();
        assert!(matches!(cfg, BackendConfig::Local(_)));
    }

    #[test]
    fn explicitly_configured_bools_keep_the_existing_lock_shape() {
        let config = resolve(
            S3_BACKEND_TYPE,
            HashMap::from([
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".into())),
                ),
                (
                    "encrypt".to_string(),
                    Value::Concrete(ConcreteValue::Bool(false)),
                ),
                (
                    "auto_create".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                ),
            ]),
        );
        let lock = BackendLock::from_config(&config);
        assert_eq!(
            lock.attributes.get("encrypt"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            lock.attributes.get("auto_create"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(lock.to_state_config().unwrap(), config);
    }

    #[test]
    fn omitted_default_bools_remain_omitted_from_lock() {
        let lock = BackendLock::from_config(&make_config("b", "us-east-1"));
        assert!(!lock.attributes.contains_key("encrypt"));
        assert!(!lock.attributes.contains_key("auto_create"));
        let settings = lock.to_state_config().unwrap();
        let settings = settings.as_s3().unwrap();
        assert!(settings.encrypt());
        assert!(settings.auto_create());
    }

    #[test]
    fn load_normalizes_valid_plain_string_and_bool_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({
            "backend_type": S3_BACKEND_TYPE,
            "attributes": {
                "bucket": "b",
                "key": "state.json",
                "region": "us_west_2",
                "encrypt": false,
                "auto_create": true
            }
        });
        std::fs::write(
            BackendLock::lock_path(tmp.path()),
            serde_json::to_string(&raw).unwrap(),
        )
        .unwrap();
        let expected_config = resolve(
            S3_BACKEND_TYPE,
            HashMap::from([
                (
                    "bucket".to_string(),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ),
                (
                    "key".to_string(),
                    Value::Concrete(ConcreteValue::String("state.json".into())),
                ),
                (
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::String("us_west_2".into())),
                ),
                (
                    "encrypt".to_string(),
                    Value::Concrete(ConcreteValue::Bool(false)),
                ),
                (
                    "auto_create".to_string(),
                    Value::Concrete(ConcreteValue::Bool(true)),
                ),
            ]),
        );

        let loaded = BackendLock::load(tmp.path()).unwrap().unwrap();

        assert_eq!(loaded, BackendLock::from_config(&expected_config));
    }

    #[test]
    fn local_default_differs_from_remote() {
        let local = BackendLock::local_default();
        assert_eq!(local.backend_type, LOCAL_BACKEND_TYPE);
        assert!(local.attributes.is_empty());
        let s3 = BackendLock::from_config(&make_config("b", "us-east-1"));
        assert_ne!(local, s3);
        let diff = local.describe_diff(&s3);
        assert!(diff.contains("backend type"));
        assert!(diff.contains(LOCAL_BACKEND_TYPE));
        assert!(diff.contains(S3_BACKEND_TYPE));
    }

    #[test]
    fn null_lock_value_is_not_coerced_to_false() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({
            "backend_type": S3_BACKEND_TYPE,
            "attributes": {
                "bucket": "b",
                "key": "state.json",
                "encrypt": null
            }
        });
        std::fs::write(
            BackendLock::lock_path(tmp.path()),
            serde_json::to_string(&raw).unwrap(),
        )
        .unwrap();
        let error = BackendLock::load(tmp.path()).unwrap_err();
        assert!(error.to_string().contains("encrypt"), "{error}");
        assert!(error.to_string().contains("string or bool"), "{error}");
    }

    #[test]
    fn obsolete_enum_region_lock_is_rejected_with_recovery_guidance() {
        let tmp = tempfile::tempdir().unwrap();
        let path = BackendLock::lock_path(tmp.path());
        let raw = serde_json::json!({
            "backend_type": S3_BACKEND_TYPE,
            "attributes": {
                "bucket": "b",
                "key": "state.json",
                "region": null
            }
        });
        std::fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();
        let error = BackendLock::load(tmp.path()).unwrap_err();
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("region"), "{message}");
        assert!(
            message.contains("records a backend address this release cannot read"),
            "{message}"
        );
        assert!(message.contains("verify the backend block"), "{message}");
        assert!(
            message.contains(&format!("delete `{}`", path.display())),
            "{message}"
        );
        let init_command = ProjectCommand::new("init", tmp.path()).to_string();
        assert!(
            message.contains(&format!("run `{init_command}` to record it again")),
            "{message}"
        );
        let BackendError::UnreadableBackendAddress {
            error: inner_error, ..
        } = &error
        else {
            panic!("invalid lock must retain its typed inner error, got {error:?}");
        };
        assert!(matches!(
            inner_error.as_ref(),
            BackendError::Serialization(_)
        ));
        assert_eq!(message.matches("Invalid backend lock attribute").count(), 1);
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn obsolete_legacy_lock_guidance_names_the_legacy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = BackendLock::legacy_lock_path(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let raw = serde_json::json!({
            "backend_type": S3_BACKEND_TYPE,
            "attributes": {
                "bucket": "b",
                "key": "state.json",
                "region": null
            }
        });
        std::fs::write(&path, serde_json::to_string(&raw).unwrap()).unwrap();

        let error = BackendLock::load(tmp.path()).unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(&format!("delete `{}`", path.display())),
            "guidance must name the lock that failed: {message}"
        );
    }
}
