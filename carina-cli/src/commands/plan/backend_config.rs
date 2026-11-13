use carina_core::parser::ExpandedBackendConfig;

/// Decoded saved-plan candidate, never backend construction authority.
/// The legacy wire omission is serialization metadata; the candidate itself
/// always names a backend and crosses state configuration admission before use.
#[derive(Debug, Clone)]
pub struct SavedBackendConfig {
    candidate: ExpandedBackendConfig,
    recorded: bool,
}
impl SavedBackendConfig {
    pub fn implicit_local() -> Self {
        Self {
            candidate: ExpandedBackendConfig {
                backend_type: "local".into(),
                attributes: Default::default(),
                attribute_spans: Default::default(),
            },
            recorded: false,
        }
    }
    pub fn from_read_backend(backend: &carina_state::ResolvedReadBackend) -> Self {
        Self {
            candidate: backend.selected_config().to_parser_config(),
            recorded: match backend.origin() {
                carina_state::ReadBackendOrigin::Implicit => false,
                carina_state::ReadBackendOrigin::Declared
                | carina_state::ReadBackendOrigin::Locked => true,
            },
        }
    }
    pub fn candidate(&self) -> &ExpandedBackendConfig {
        &self.candidate
    }
    pub(super) fn into_wire(self) -> Option<ExpandedBackendConfig> {
        self.recorded.then_some(self.candidate)
    }
}
impl From<Option<ExpandedBackendConfig>> for SavedBackendConfig {
    fn from(candidate: Option<ExpandedBackendConfig>) -> Self {
        match candidate {
            Some(candidate) => Self {
                candidate,
                recorded: true,
            },
            None => Self::implicit_local(),
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
