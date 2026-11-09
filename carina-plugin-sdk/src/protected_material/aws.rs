//! Registered AWS snapshots and the source-backed signing provider.
use super::RegistrationError;
use crate::ProviderDiagnostics;
use aws_credential_types::{Credentials, provider::error::CredentialsError};
use carina_provider_protocol::diagnostics as wire;
use std::{future::Future, pin::Pin, time::SystemTime};

/// One credential generation, including the expiry used by the AWS SDK cache.
#[derive(Clone)]
pub struct AwsSessionMaterial {
    values: wire::AwsSession,
    expires_at: Option<SystemTime>,
}
impl AwsSessionMaterial {
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        expires_at: Option<SystemTime>,
    ) -> Self {
        Self {
            values: wire::AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            },
            expires_at,
        }
    }
}

/// Result of asking a source for its current credential generation.
pub type AwsSessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AwsSessionMaterial, CredentialsError>> + Send + 'a>>;

/// Supplies the current credentials when the AWS SDK asks for a refresh.
/// The SDK registers each successful generation before returning signing credentials.
/// Source errors retain their typed AWS credentials error and source chain.
pub trait AwsSessionSource: Send + Sync + 'static {
    fn current_session(&self) -> AwsSessionFuture<'_>;
}

// Only registration acknowledgement mints this internal, consuming conversion.
// It cannot be installed as an AWS credentials provider or prepare HTTP requests.
pub(crate) struct RegisteredAwsSession {
    material: AwsSessionMaterial,
}
impl ProviderDiagnostics {
    pub(crate) fn register_aws_session(
        &self,
        material: AwsSessionMaterial,
    ) -> Result<RegisteredAwsSession, RegistrationError> {
        self.register(wire::ProtectedMaterial::AwsSession(material.values.clone()))?;
        Ok(RegisteredAwsSession { material })
    }
}
impl RegisteredAwsSession {
    pub(crate) fn into_credentials(self) -> Credentials {
        let AwsSessionMaterial { values, expires_at } = self.material;
        let wire::AwsSession {
            access_key_id,
            secret_access_key,
            session_token,
        } = values;
        Credentials::new(
            access_key_id,
            secret_access_key,
            session_token,
            expires_at,
            "carina-registered-session",
        )
    }
}
