//! Local backend-configuration lock file for change detection.
//!
//! Carina stores a hash of the current `backend` block in a local file
//! (`carina-backend.lock`) at the project root. Before
//! each plan/apply, the stored hash is compared against the current
//! configuration — a mismatch indicates that the backend has been
//! reconfigured (for example, the bucket or key was changed), and Carina
//! refuses to proceed without an explicit `--reconfigure` override.
//!
//! This prevents silently pointing state operations at a different state
//! file, which could lead to resources being treated as unmanaged or
//! data loss.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::backend::{BackendConfig, BackendError, BackendResult};

/// Name of the lock file at the project root.
pub const LOCK_FILE: &str = "carina-backend.lock";

/// Legacy lock path components (for migration from `.carina/backend-lock.json`).
const LEGACY_LOCK_DIR: &str = ".carina";
const LEGACY_LOCK_FILE: &str = "backend-lock.json";

/// Snapshot of the backend configuration that was last used for a given
/// configuration root. Persisted to disk as JSON.
///
/// Attribute values are stored as `serde_json::Value` so that any DSL
/// type can be serialized (strings, bools, numbers). Keys are sorted in
/// a `BTreeMap` so the serialized form is deterministic regardless of
/// `HashMap` iteration order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendLock {
    /// Backend type identifier (e.g., `"s3"`, `"local"`).
    pub backend_type: String,
    /// Sorted snapshot of backend attributes for stable comparison.
    pub attributes: BTreeMap<String, serde_json::Value>,
}

impl BackendLock {
    /// Build a lock snapshot from the current backend configuration.
    pub fn from_config(config: &BackendConfig) -> Self {
        let attributes = config
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect();
        Self {
            backend_type: config.backend_type.clone(),
            attributes,
        }
    }

    /// Build a lock snapshot representing the implicit local backend that
    /// is used when no `backend` block is configured. Allows
    /// `check_backend_lock` to detect local → remote transitions.
    pub fn local_default() -> Self {
        Self {
            backend_type: "local".to_string(),
            attributes: BTreeMap::new(),
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
            return Self::load_from(&path);
        }
        // Migration: try legacy path
        let legacy = Self::legacy_lock_path(base_dir);
        if legacy.exists() {
            return Self::load_from(&legacy);
        }
        Ok(None)
    }

    fn load_from(path: &Path) -> BackendResult<Option<Self>> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| BackendError::Io(format!("Failed to read {}: {}", path.display(), e)))?;
        let lock: Self = serde_json::from_str(&contents).map_err(|e| {
            BackendError::Serialization(format!(
                "Failed to parse backend lock at {}: {}",
                path.display(),
                e
            ))
        })?;
        Ok(Some(lock))
    }

    /// Persist this lock snapshot at the project root.
    pub fn save(&self, base_dir: &Path) -> BackendResult<()> {
        let path = Self::lock_path(base_dir);
        let contents = serde_json::to_string_pretty(self)
            .map_err(|e| BackendError::Serialization(e.to_string()))?;
        std::fs::write(&path, contents)
            .map_err(|e| BackendError::Io(format!("Failed to write {}: {}", path.display(), e)))?;
        Ok(())
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

/// Convert a `carina_core::resource::Value` into a `serde_json::Value`
/// for persistence in the lock file. Only scalar types are expected in
/// backend configurations, but complex types fall back to `null`.
fn value_to_json(value: &carina_core::resource::Value) -> serde_json::Value {
    use carina_core::resource::Value;
    match value {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        // RFC #2371: `Value::Unknown` is plan-display only and must
        // never reach a state file. The wildcard below would silently
        // map it to `null`; reject explicitly so a stage-2/3 producer
        // bug surfaces here instead of silently corrupting the lock.
        Value::Unknown(_) => {
            unimplemented!(
                "Value::Unknown reached a stage-4 serialization boundary; the producer should have stripped or resolved it (RFC #2371)"
            )
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::resource::Value;
    use std::collections::HashMap;

    fn make_config(bucket: &str, region: &str) -> BackendConfig {
        let mut attributes = HashMap::new();
        attributes.insert("bucket".to_string(), Value::String(bucket.to_string()));
        attributes.insert("region".to_string(), Value::String(region.to_string()));
        BackendConfig {
            backend_type: "s3".to_string(),
            attributes,
        }
    }

    #[test]
    fn from_config_captures_type_and_attributes() {
        let config = make_config("my-bucket", "us-east-1");
        let lock = BackendLock::from_config(&config);
        assert_eq!(lock.backend_type, "s3");
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
    fn local_default_differs_from_remote() {
        let local = BackendLock::local_default();
        assert_eq!(local.backend_type, "local");
        assert!(local.attributes.is_empty());
        let s3 = BackendLock::from_config(&make_config("b", "us-east-1"));
        assert_ne!(local, s3);
        let diff = local.describe_diff(&s3);
        assert!(diff.contains("backend type"));
        assert!(diff.contains("local"));
        assert!(diff.contains("s3"));
    }

    /// RFC #2371 contract pin: `value_to_json` panics on
    /// `Value::Unknown`. Backend lock files must never carry the
    /// variant (constraint b); a future change that swaps for a silent
    /// fallback would re-introduce v1-style corruption.
    #[test]
    #[should_panic(expected = "Value::Unknown")]
    fn unknown_panics_in_value_to_json() {
        use carina_core::resource::{AccessPath, UnknownReason, Value};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        let v = Value::Unknown(UnknownReason::UpstreamRef { path });
        let _ = value_to_json(&v);
    }
}
