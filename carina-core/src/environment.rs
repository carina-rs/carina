//! Owned environment inputs supplied by the executable boundary.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::sync::Arc;

/// An immutable environment shared by all evaluations in one invocation.
///
/// Executables capture process inputs once; libraries receive the snapshot
/// from their caller and tests supply their own values.
#[derive(Clone)]
pub struct EnvironmentSnapshot {
    values: Arc<BTreeMap<String, OsString>>,
}

impl EnvironmentSnapshot {
    /// Capture the process environment at an executable entry point.
    /// Non-Unicode keys cannot be named by `env()` and are omitted. Values
    /// retain their native OS representation until the consumer decodes them.
    pub fn capture_process() -> Self {
        Self::new(
            std::env::vars_os()
                .filter_map(|(key, value)| key.into_string().ok().map(|key| (key, value))),
        )
    }

    pub fn new(values: impl IntoIterator<Item = (String, OsString)>) -> Self {
        Self {
            values: Arc::new(values.into_iter().collect()),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn empty() -> Self {
        Self::new([])
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.values.get(name).map(OsString::as_os_str)
    }
}

impl std::fmt::Debug for EnvironmentSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentSnapshot")
            .field("variable_count", &self.values.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_snapshot_owns_values_and_redacts_context_debug() {
        let mut values = BTreeMap::from([("TOKEN".to_string(), OsString::from("original-secret"))]);
        let snapshot = EnvironmentSnapshot::new(values.clone());
        values.insert("TOKEN".to_string(), OsString::from("changed-secret"));
        let cloned = snapshot.clone();
        drop(snapshot);
        assert_eq!(cloned.get("TOKEN"), Some(OsStr::new("original-secret")));

        let context = crate::parser::ProviderContext::new(cloned);
        let debug = format!("{context:?}");
        assert!(debug.contains("variable_count: 1"));
        assert!(!debug.contains("original-secret"));
        assert!(!debug.contains("changed-secret"));

        let loaded = crate::parser::LoadedProviderContext::empty_for_test().with_runtime(&context);
        assert_eq!(
            loaded.environment.get("TOKEN"),
            Some(OsStr::new("original-secret"))
        );
        assert!(loaded.custom_types_loaded());
        assert!(!format!("{loaded:?}").contains("original-secret"));
    }
}
