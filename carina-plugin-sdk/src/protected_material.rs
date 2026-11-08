//! Confidential adoption. Only acknowledged registration mints operational wrappers.
use crate::ProviderDiagnostics;
use carina_provider_protocol::diagnostics as wire;
pub use wire::RegistrationError;

#[derive(Debug)]
pub enum RequestPreparationError {
    Registration(RegistrationError),
    InvalidHeader(http::header::InvalidHeaderValue),
    InvalidText(std::str::Utf8Error),
}
impl std::fmt::Display for RequestPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Registration(_error) => "request protection registration failed",
            Self::InvalidHeader(_error) => "invalid authorization header",
            Self::InvalidText(_error) => "request protection material is not UTF-8",
        })
    }
}
impl std::error::Error for RequestPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Registration(error) => error,
            Self::InvalidHeader(error) => error,
            Self::InvalidText(error) => error,
        })
    }
}

pub struct AwsSessionMaterial(wire::AwsSession);
impl AwsSessionMaterial {
    pub fn new(
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    ) -> Self {
        Self(wire::AwsSession {
            access_key_id,
            secret_access_key,
            session_token,
        })
    }
}
pub struct GithubTokenMaterial(String);
impl GithubTokenMaterial {
    pub fn new(token: String) -> Self {
        Self(token)
    }
}
pub struct SigningMaterial(wire::SignedMaterial);
impl SigningMaterial {
    pub fn new(
        authorization: String,
        signature: Option<String>,
        query_secret_values: Vec<String>,
    ) -> Self {
        Self(wire::SignedMaterial {
            authorization,
            signature,
            query_secret_values,
        })
    }
}

/// No external constructor, serialization, or plaintext accessor exists.
///
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredAwsSession;
/// let _ = RegisteredAwsSession { material: todo!(), diagnostics: todo!() };
/// ```
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredGithubToken;
/// let _ = RegisteredGithubToken { token: String::new(), diagnostics: todo!() };
/// ```
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredSignedMaterial;
/// let _ = RegisteredSignedMaterial { diagnostics: todo!() };
/// ```
/// ```compile_fail,E0277
/// use carina_plugin_sdk::protected_material::RegisteredGithubToken;
/// fn expose(token: RegisteredGithubToken) { let _ = format!("{token}"); }
/// ```
/// ```compile_fail,E0599
/// use carina_plugin_sdk::protected_material::RegisteredAwsSession;
/// fn expose(session: RegisteredAwsSession) { let _ = session.secret_access_key(); }
/// ```
/// ```
/// use carina_core::provider::ProviderPlan;
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::GithubTokenMaterial};
/// let session = ProviderPlan::state_only().session("example", None, None);
/// let diagnostics = ProviderDiagnostics::from_session(&session);
/// let token = diagnostics.register_github_token(GithubTokenMaterial::new("token".into())).unwrap();
/// assert!(!format!("{token:?}").contains("token"));
/// ```
#[derive(Clone)]
pub struct RegisteredAwsSession {
    pub(crate) material: AwsSessionMaterial,
    pub(crate) diagnostics: ProviderDiagnostics,
}
#[derive(Clone)]
pub struct RegisteredGithubToken {
    token: String,
    diagnostics: ProviderDiagnostics,
}
pub struct RegisteredSignedMaterial {
    pub(crate) diagnostics: ProviderDiagnostics,
}

impl Clone for AwsSessionMaterial {
    fn clone(&self) -> Self {
        Self::new(
            self.0.access_key_id.clone(),
            self.0.secret_access_key.clone(),
            self.0.session_token.clone(),
        )
    }
}
macro_rules! opaque_debug {
    ($($ty:ty),*) => { $(impl std::fmt::Debug for $ty {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("<registered protected material>") }
    })* };
}
opaque_debug!(
    RegisteredAwsSession,
    RegisteredGithubToken,
    RegisteredSignedMaterial
);

impl ProviderDiagnostics {
    pub fn register_aws_session(
        &self,
        material: AwsSessionMaterial,
    ) -> Result<RegisteredAwsSession, RegistrationError> {
        self.register(wire::ProtectedMaterial::AwsSession(wire::AwsSession {
            access_key_id: material.0.access_key_id.clone(),
            secret_access_key: material.0.secret_access_key.clone(),
            session_token: material.0.session_token.clone(),
        }))?;
        Ok(RegisteredAwsSession {
            material,
            diagnostics: self.clone(),
        })
    }
    pub fn register_github_token(
        &self,
        material: GithubTokenMaterial,
    ) -> Result<RegisteredGithubToken, RegistrationError> {
        self.register(wire::ProtectedMaterial::GithubToken(material.0.clone()))?;
        Ok(RegisteredGithubToken {
            token: material.0,
            diagnostics: self.clone(),
        })
    }
    pub fn register_signing_material(
        &self,
        material: SigningMaterial,
    ) -> Result<RegisteredSignedMaterial, RegistrationError> {
        self.register(wire::ProtectedMaterial::SignedMaterial(material.0))?;
        Ok(RegisteredSignedMaterial {
            diagnostics: self.clone(),
        })
    }
}

impl aws_credential_types::provider::ProvideCredentials for RegisteredAwsSession {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
            aws_credential_types::Credentials::new(
                self.material.0.access_key_id.clone(),
                self.material.0.secret_access_key.clone(),
                self.material.0.session_token.clone(),
                None,
                "carina-registered-session",
            ),
        ))
    }
}

/// Transport input minted only after registration has acknowledged every signing field.
///
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredRequest;
/// let _ = RegisteredRequest { request: todo!(), registration: todo!() };
/// ```
pub struct RegisteredRequest {
    pub(crate) request: http::Request<Vec<u8>>,
    pub(crate) registration: RegisteredSignedMaterial,
}
impl std::fmt::Debug for RegisteredRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredRequest")
            .field("body_bytes", &self.request.body().len())
            .field("registration", &self.registration)
            .finish()
    }
}
impl ProviderDiagnostics {
    pub fn prepare_request(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<RegisteredRequest, RequestPreparationError> {
        let headers = request
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let registration = self.register_request_material(&request.uri().to_string(), &headers)?;
        Ok(RegisteredRequest {
            request,
            registration,
        })
    }
    pub(crate) fn register_request_material(
        &self,
        uri: &str,
        headers: &[(String, Vec<u8>)],
    ) -> Result<RegisteredSignedMaterial, RequestPreparationError> {
        let mut authorization = String::new();
        let mut secrets = Vec::new();
        let mut signature = None;
        for (name, value) in headers {
            if name.eq_ignore_ascii_case("authorization") {
                let value =
                    std::str::from_utf8(value).map_err(RequestPreparationError::InvalidText)?;
                authorization = value.to_owned();
                for part in value.split([',', ' ']) {
                    if let Some(value) = part.strip_prefix("Signature=") {
                        signature = Some(value.to_owned());
                    }
                    if let Some(value) = part.strip_prefix("Credential=") {
                        secrets.push(value.split('/').next().unwrap_or(value).to_owned());
                    }
                }
                if let Some(token) = value.strip_prefix("Bearer ") {
                    secrets.push(token.to_owned());
                }
            } else if name.eq_ignore_ascii_case("x-amz-security-token")
                || name.eq_ignore_ascii_case("cookie")
                || name.eq_ignore_ascii_case("proxy-authorization")
            {
                secrets.push(
                    std::str::from_utf8(value)
                        .map_err(RequestPreparationError::InvalidText)?
                        .to_owned(),
                );
            }
        }
        if let Some(query) = uri
            .split_once('?')
            .map(|(_base, query)| query.split('#').next().unwrap_or(query))
        {
            for parameter in query.split('&') {
                if let Some((_name, value)) = parameter.split_once('=') {
                    let decoded = percent_encoding::percent_decode_str(value)
                        .decode_utf8()
                        .map_err(RequestPreparationError::InvalidText)?;
                    secrets.push(decoded.into_owned());
                }
            }
        }
        self.register_signing_material(SigningMaterial::new(authorization, signature, secrets))
            .map_err(RequestPreparationError::Registration)
    }
}
impl RegisteredGithubToken {
    pub fn authorize(
        &self,
        mut request: http::Request<Vec<u8>>,
    ) -> Result<RegisteredRequest, RequestPreparationError> {
        let value = http::HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(RequestPreparationError::InvalidHeader)?;
        request
            .headers_mut()
            .insert(http::header::AUTHORIZATION, value);
        self.diagnostics.prepare_request(request)
    }
}
impl RegisteredSignedMaterial {
    pub fn prepare_request(
        self,
        request: http::Request<Vec<u8>>,
    ) -> Result<RegisteredRequest, RequestPreparationError> {
        self.diagnostics.prepare_request(request)
    }
}
impl RegisteredAwsSession {
    pub fn prepare_request(
        &self,
        request: http::Request<Vec<u8>>,
    ) -> Result<RegisteredRequest, RequestPreparationError> {
        self.diagnostics.prepare_request(request)
    }
}
