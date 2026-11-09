//! Registered AWS snapshots and the source-backed signing provider.
use super::RegistrationError;
use crate::ProviderDiagnostics;
use aws_credential_types::{Credentials, provider::error::CredentialsError};
use carina_provider_protocol::diagnostics as wire;
use std::{future::Future, pin::Pin};

/// One SDK credential generation, retained whole through registration.
#[derive(Clone)]
pub struct AwsSessionMaterial {
    credentials: Credentials,
}
impl AwsSessionMaterial {
    pub fn new(credentials: Credentials) -> Self {
        Self { credentials }
    }
}
impl std::fmt::Debug for AwsSessionMaterial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<protected AWS session material>")
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
        self.register(wire::ProtectedMaterial::AwsSession(wire::AwsSession {
            access_key_id: material.credentials.access_key_id().to_owned(),
            secret_access_key: material.credentials.secret_access_key().to_owned(),
            session_token: material.credentials.session_token().map(str::to_owned),
        }))?;
        Ok(RegisteredAwsSession { material })
    }
}
impl RegisteredAwsSession {
    pub(crate) fn into_credentials(self) -> Credentials {
        self.material.credentials
    }
}
