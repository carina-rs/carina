use carina_core::parser::ExpandedBackendConfig;

/// Decoded saved-plan selection, never backend construction authority.
/// Execution enters through `selection_source()`; wire serialization retains
/// the optional candidate without exposing its stored payload directly.
///
/// ```compile_fail
/// use carina_cli::commands::plan::SavedBackendConfig;
/// fn bypass(config: &SavedBackendConfig) {
///     let _ = &config.source; // forbidden: the stored payload is private
/// }
/// ```
/// ```
/// # use carina_cli::commands::plan::SavedBackendConfig;
/// # fn bypass(config: &SavedBackendConfig) {
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct SavedBackendConfig {
    source: SavedBackendSource,
}
#[derive(Debug, Clone, Default)]
enum SavedBackendSource {
    #[default]
    Implicit,
    Recorded(ExpandedBackendConfig),
}
impl SavedBackendConfig {
    pub(super) fn from_read_backend(backend: &carina_state::ResolvedReadBackend) -> Self {
        Self {
            source: match backend.origin() {
                carina_state::ReadBackendOrigin::Implicit => SavedBackendSource::Implicit,
                carina_state::ReadBackendOrigin::Declared
                | carina_state::ReadBackendOrigin::Locked => {
                    SavedBackendSource::Recorded(backend.selected_config().to_parser_config())
                }
            },
        }
    }
    pub fn selection_source(&self) -> carina_state::BackendConfigurationSource<'_> {
        match &self.source {
            SavedBackendSource::Implicit => carina_state::BackendConfigurationSource::Implicit,
            SavedBackendSource::Recorded(candidate) => {
                carina_state::BackendConfigurationSource::Declared(candidate)
            }
        }
    }
    pub(super) fn candidate(&self) -> Option<&ExpandedBackendConfig> {
        match &self.source {
            SavedBackendSource::Implicit => None,
            SavedBackendSource::Recorded(candidate) => Some(candidate),
        }
    }
    pub(super) fn into_wire(self) -> Option<ExpandedBackendConfig> {
        match self.source {
            SavedBackendSource::Implicit => None,
            SavedBackendSource::Recorded(candidate) => Some(candidate),
        }
    }
}
impl From<Option<ExpandedBackendConfig>> for SavedBackendConfig {
    fn from(candidate: Option<ExpandedBackendConfig>) -> Self {
        Self {
            source: match candidate {
                Some(candidate) => SavedBackendSource::Recorded(candidate),
                None => SavedBackendSource::Implicit,
            },
        }
    }
}
impl serde::Serialize for SavedBackendConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.candidate(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for SavedBackendConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Option<ExpandedBackendConfig> as serde::Deserialize>::deserialize(deserializer)
            .map(Self::from)
    }
}

#[cfg(test)]
mod read_origin_serialization_tests {
    use super::*;
    use carina_core::paths::ResolvedPath;
    fn raw(path: &str) -> ExpandedBackendConfig {
        ExpandedBackendConfig {
            source: (),
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
    async fn saved_backend_bytes_follow_read_origin() {
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
mod implicit_backend_serialization_tests {
    use super::*;
    #[test]
    fn saved_backend_implicit_has_no_candidate() {
        let saved = SavedBackendConfig::default();
        assert!(matches!(
            saved.selection_source(),
            carina_state::BackendConfigurationSource::Implicit
        ));
        assert_eq!(serde_json::to_string(&saved.into_wire()).unwrap(), "null");
    }
}

#[cfg(test)]
mod saved_backend_serde_tests {
    use super::*;
    #[test]
    fn round19_saved_upstream_requires_relative_caller() {
        let source = super::super::UpstreamSource {
            binding: carina_core::resource::absolute_binding_address_for_test("prior"),
            source: "../source".into(),
            caller: carina_core::parser::UpstreamCallSite::for_test(),
        };
        let valid = serde_json::to_value(source).unwrap();
        serde_json::from_value::<super::super::UpstreamSource>(valid.clone()).unwrap();
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("caller");
        assert!(serde_json::from_value::<super::super::UpstreamSource>(missing).is_err());
        let mut absolute = valid;
        absolute["caller"]["file"] = serde_json::json!("/local/private/project/main.crn");
        assert!(serde_json::from_value::<super::super::UpstreamSource>(absolute).is_err());
    }

    #[test]
    fn round19_saved_backend_deserialization_uses_plain_value_errors() {
        let expected = serde_json::from_value::<
            carina_core::resource::Value<
                carina_core::resource::NoPlaceholder,
                carina_core::resource::AbsoluteBindingAddress,
            >,
        >(serde_json::Value::Null)
        .unwrap_err();
        let error = serde_json::from_value::<SavedBackendConfig>(
            serde_json::json!({"backend_type":"s3", "attributes":{"bucket":null, "key":"k"}}),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), expected.to_string());
    }
    #[test]
    fn saved_backend_serde_preserves_private_origin_payload() {
        let recorded = ExpandedBackendConfig {
            source: (),
            backend_type: "local".into(),
            attributes: [(
                "path".to_owned(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("state.json".into()),
                ),
            )]
            .into_iter()
            .collect(),
            attribute_spans: Default::default(),
        };
        for wire in [
            serde_json::Value::Null,
            serde_json::to_value(recorded).unwrap(),
        ] {
            let saved: SavedBackendConfig = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(serde_json::to_value(&saved).unwrap(), wire);
        }
    }
}
