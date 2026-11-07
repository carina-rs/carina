use super::{DiagnosticText, ProviderText, RedactionSet};
use std::{cmp::Ordering, fmt};

/// An IAM action admitted by the provider boundary. Display uses that
/// boundary's inventory; plaintext is available only at the IAM request seam.
/// Admission accepts exactly `[A-Za-z0-9-]+:[A-Za-z0-9*.]+` in ASCII,
/// preserving service-prefix case and dots in provider-declared actions.
#[derive(Clone)]
pub struct RequiredPermission {
    action: String,
    display: DiagnosticText,
}

/// A rejected permission is identified by position, never by its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid required permission at index {index}")]
pub struct InvalidRequiredPermission {
    index: usize,
}
impl InvalidRequiredPermission {
    pub fn index(&self) -> usize {
        self.index
    }
}

impl RequiredPermission {
    pub(crate) fn admit(
        action: String,
        index: usize,
        secrets: &RedactionSet,
    ) -> Result<Self, InvalidRequiredPermission> {
        let valid = action.split_once(':').is_some_and(|(service, action)| {
            !service.is_empty()
                && !action.is_empty()
                && service
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && action
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'*' || b == b'.')
        });
        if !valid {
            return Err(InvalidRequiredPermission { index });
        }
        Ok(Self {
            display: DiagnosticText::new(
                secrets.redact(&ProviderText::new(action.clone())).as_str(),
            ),
            action,
        })
    }
    /// Use only to populate an IAM SimulatePrincipalPolicy request. Diagnostics
    /// must use Display; response/policy matching needs no plaintext accessor.
    pub fn iam_action(&self) -> &str {
        &self.action
    }
    pub fn matches_iam_action(&self, action: &str) -> bool {
        self.action == action
    }
    pub fn matches_policy_pattern(&self, pattern: &str) -> bool {
        let pattern = pattern.to_ascii_lowercase();
        let action = self.action.to_ascii_lowercase();
        pattern == "*"
            || pattern == action
            || pattern
                .strip_suffix('*')
                .is_some_and(|prefix| action.starts_with(prefix))
    }
}
impl fmt::Display for RequiredPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.display.fmt(f)
    }
}
impl fmt::Debug for RequiredPermission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.display, f)
    }
}
impl PartialEq for RequiredPermission {
    fn eq(&self, other: &Self) -> bool {
        self.action == other.action
    }
}
impl Eq for RequiredPermission {}
impl PartialEq<String> for RequiredPermission {
    fn eq(&self, other: &String) -> bool {
        self.matches_iam_action(other)
    }
}
impl Ord for RequiredPermission {
    fn cmp(&self, other: &Self) -> Ordering {
        self.action.cmp(&other.action)
    }
}
impl PartialOrd for RequiredPermission {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
