//! Host-resolved AWS credentials for AWS-kind provider guests.

use std::fmt;
use std::time::SystemTime;
#[cfg(target_arch = "wasm32")]
use std::time::{Duration, UNIX_EPOCH};

#[derive(Clone, PartialEq, Eq)]
struct SecretString(String);

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// The current base AWS credentials supplied by the Carina host.
#[derive(Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: SecretString,
    session_token: Option<SecretString>,
    expires_at: Option<SystemTime>,
}

impl AwsCredentials {
    #[cfg(any(target_arch = "wasm32", test))]
    fn new(
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        expires_at: Option<SystemTime>,
    ) -> Self {
        Self {
            access_key_id,
            secret_access_key: SecretString(secret_access_key),
            session_token: session_token.map(SecretString),
            expires_at,
        }
    }

    /// Returns the AWS access key ID.
    pub fn access_key_id(&self) -> &str {
        &self.access_key_id
    }

    /// Returns the AWS secret access key.
    pub fn secret_access_key(&self) -> &str {
        &self.secret_access_key.0
    }

    /// Returns the AWS session token, when the source supplied one.
    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_ref().map(|value| value.0.as_str())
    }

    /// Returns the credential expiration time, when known.
    pub fn expires_at(&self) -> Option<SystemTime> {
        self.expires_at
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsCredentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &self.secret_access_key)
            .field("session_token", &self.session_token)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// A typed failure to obtain host-resolved AWS credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AwsCredentialError {
    /// No provider in the host's AWS credential chain produced credentials.
    NotFound(String),
    /// A selected provider failed while resolving or refreshing credentials.
    ProviderFailed(String),
}

impl fmt::Display for AwsCredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(message) => {
                write!(formatter, "AWS credentials were not found: {message}")
            }
            Self::ProviderFailed(message) => {
                write!(formatter, "the AWS credential provider failed: {message}")
            }
        }
    }
}

impl std::error::Error for AwsCredentialError {}

/// Requests the current host-resolved base credentials.
///
/// Native provider builds receive a typed error because the import exists only
/// in an AWS-world Wasm guest.
#[cfg(target_arch = "wasm32")]
pub fn current() -> Result<AwsCredentials, AwsCredentialError> {
    use carina_plugin_bindings_aws_credentials::carina::provider::aws_credentials as wit;

    let credentials = wit::current_credentials().map_err(|error| match error {
        wit::AwsCredentialError::NotFound(message) => AwsCredentialError::NotFound(message),
        wit::AwsCredentialError::ProviderFailed(message) => {
            AwsCredentialError::ProviderFailed(message)
        }
    })?;
    let expires_at = match credentials.expires_at {
        Some(seconds) => Some(
            UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| {
                    AwsCredentialError::ProviderFailed(
                        "credential expiration is outside the supported time range".to_string(),
                    )
                })?,
        ),
        None => None,
    };

    Ok(AwsCredentials::new(
        credentials.access_key_id,
        credentials.secret_access_key,
        credentials.session_token,
        expires_at,
    ))
}

/// Requests the current host-resolved base credentials.
///
/// Native provider builds receive a typed error because the import exists only
/// in an AWS-world Wasm guest.
#[cfg(not(target_arch = "wasm32"))]
pub fn current() -> Result<AwsCredentials, AwsCredentialError> {
    Err(AwsCredentialError::ProviderFailed(
        "AWS credentials are available only to wasm32 AWS-kind provider guests".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_current_is_a_typed_error() {
        assert!(matches!(
            current(),
            Err(AwsCredentialError::ProviderFailed(_))
        ));
    }

    #[test]
    fn debug_redacts_secret_fields() {
        let credentials = AwsCredentials::new(
            "AKIAEXAMPLE".to_string(),
            "sdk-secret-sentinel".to_string(),
            Some("sdk-session-sentinel".to_string()),
            None,
        );
        let rendered = format!("{credentials:?}");
        assert!(rendered.contains("AKIAEXAMPLE"));
        assert!(!rendered.contains("sdk-secret-sentinel"));
        assert!(!rendered.contains("sdk-session-sentinel"));
    }
}
