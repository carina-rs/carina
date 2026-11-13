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
