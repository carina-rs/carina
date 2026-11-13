use carina_core::parser::ExpandedBackendConfig;

/// Decoded saved-plan candidate, never backend construction authority.
/// Implicit selection has no candidate; a recorded candidate crosses state
/// configuration admission before use.
/// Selection consumes `selection_source()` so wire omission retains its origin.
///
/// ```compile_fail
/// use carina_cli::commands::plan::SavedBackendConfig;
/// fn bypass(config: &SavedBackendConfig) {
///     let _ = config.candidate(); // forbidden: only serialization can access the bare candidate
/// }
/// ```
/// ```
/// # use carina_cli::commands::plan::SavedBackendConfig;
/// # fn bypass(config: &SavedBackendConfig) {
/// # }
/// ```
#[derive(Debug, Clone)]
pub enum SavedBackendConfig {
    Implicit,
    Recorded(ExpandedBackendConfig),
}
impl SavedBackendConfig {
    pub fn implicit_local() -> Self {
        Self::Implicit
    }
    pub fn from_read_backend(backend: &carina_state::ResolvedReadBackend) -> Self {
        match backend.origin() {
            carina_state::ReadBackendOrigin::Implicit => Self::Implicit,
            carina_state::ReadBackendOrigin::Declared | carina_state::ReadBackendOrigin::Locked => {
                Self::Recorded(backend.selected_config().to_parser_config())
            }
        }
    }
    pub fn selection_source(&self) -> carina_state::BackendConfigurationSource<'_> {
        match self {
            Self::Implicit => carina_state::BackendConfigurationSource::Implicit,
            Self::Recorded(candidate) => {
                carina_state::BackendConfigurationSource::Declared(candidate)
            }
        }
    }
    pub(super) fn candidate(&self) -> Option<&ExpandedBackendConfig> {
        match self {
            Self::Implicit => None,
            Self::Recorded(candidate) => Some(candidate),
        }
    }
    pub(super) fn into_wire(self) -> Option<ExpandedBackendConfig> {
        match self {
            Self::Implicit => None,
            Self::Recorded(candidate) => Some(candidate),
        }
    }
}
impl From<Option<ExpandedBackendConfig>> for SavedBackendConfig {
    fn from(candidate: Option<ExpandedBackendConfig>) -> Self {
        match candidate {
            Some(candidate) => Self::Recorded(candidate),
            None => Self::Implicit,
        }
    }
}

#[cfg(test)]
mod review_r1_tests {
    use super::*;
    use carina_core::paths::ResolvedPath;
    fn raw(path: &str) -> ExpandedBackendConfig {
        ExpandedBackendConfig {
            backend_type: "local".into(),
            attributes: [(
                "path".to_owned(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String(path.into()),
                ),
            )]
            .into_iter()
            .collect(),
            attribute_spans: Default::default(),
        }
    }
    #[tokio::test]
    async fn review_r1_saved_backend_bytes_follow_read_origin() {
        let root = tempfile::tempdir().unwrap();
        let base = ResolvedPath::new(root.path()).unwrap();
        let declared = raw("new.json");
        for (input, expected) in [(None, None), (Some(&declared), Some(declared.clone()))] {
            let read = carina_state::BackendSelection::for_read(&base, input)
                .unwrap()
                .resolve_with_drift_warning(|_| {})
                .await
                .unwrap();
            let saved = SavedBackendConfig::from_read_backend(&read);
            assert_eq!(
                serde_json::to_vec(&saved.into_wire()).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
        }
        let old = raw("old.json");
        carina_state::BackendLock::for_config(Some(&old))
            .unwrap()
            .save(&base)
            .unwrap();
        let read = carina_state::BackendSelection::for_read(&base, Some(&declared))
            .unwrap()
            .resolve_with_drift_warning(|_| {})
            .await
            .unwrap();
        let saved = SavedBackendConfig::from_read_backend(&read);
        assert_eq!(
            serde_json::to_vec(&saved.into_wire()).unwrap(),
            serde_json::to_vec(&Some(old)).unwrap()
        );
    }
}

#[cfg(test)]
mod round3_exact_saved_backend_tests {
    use super::*;
    #[test]
    fn round3_saved_backend_implicit_has_no_candidate() {
        let saved = SavedBackendConfig::Implicit;
        assert!(matches!(
            saved.selection_source(),
            carina_state::BackendConfigurationSource::Implicit
        ));
        assert_eq!(serde_json::to_string(&saved.into_wire()).unwrap(), "null");
    }
}
