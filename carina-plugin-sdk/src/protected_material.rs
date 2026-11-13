//! Confidential adoption. Only acknowledged registration mints operational wrappers.
use crate::ProviderDiagnostics;
use carina_provider_protocol::diagnostics as wire;
pub use wire::RegistrationError;

#[derive(Debug)]
pub enum RequestPreparationError {
    InvalidHeader(http::header::InvalidHeaderValue),
}
impl std::fmt::Display for RequestPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidHeader(_error) => "invalid authorization header",
        })
    }
}
impl std::error::Error for RequestPreparationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::InvalidHeader(error) => error,
        })
    }
}

#[cfg(feature = "aws")]
// Native builds without the WASI transport expose source definitions only.
#[cfg_attr(
    not(any(target_arch = "wasm32", test, feature = "test-utils")),
    allow(dead_code, reason = "AWS material is consumed by the WASI transport")
)]
mod aws;
#[cfg(feature = "aws")]
pub use aws::{AwsSessionFuture, AwsSessionMaterial, AwsSessionSource};

pub struct GithubTokenMaterial(String);
impl GithubTokenMaterial {
    pub fn new(token: String) -> Self {
        Self(token)
    }
}
/// An acknowledged token has no external constructor or plaintext display.
///
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredGithubToken;
/// let _ = RegisteredGithubToken { token: String::new(), diagnostics: todo!() };
/// ```
/// ```compile_fail,E0277
/// use carina_plugin_sdk::protected_material::RegisteredGithubToken;
/// fn expose(token: RegisteredGithubToken) { let _ = format!("{token}"); }
/// ```
/// ```
/// use carina_core::provider::ProviderValidationInputs;
/// use carina_plugin_sdk::{ProviderDiagnostics, protected_material::{GithubTokenMaterial, RegisteredRequest}};
/// let inputs = ProviderValidationInputs::for_schema("example");
/// let diagnostics = ProviderDiagnostics::for_validation(&inputs);
/// let token = diagnostics.register_github_token(GithubTokenMaterial::new("token".into())).unwrap();
/// assert!(!format!("{token:?}").contains("token"));
/// let _: RegisteredRequest = token.authorize(http::Request::new(Vec::new())).unwrap();
/// ```
#[derive(Clone)]
pub struct RegisteredGithubToken {
    token: String,
    diagnostics: ProviderDiagnostics,
}
macro_rules! opaque_debug {
    ($($ty:ty),*) => { $(impl std::fmt::Debug for $ty {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("<registered protected material>") }
    })* };
}
opaque_debug!(RegisteredGithubToken);

impl ProviderDiagnostics {
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
}

/// Prepared transport input retaining its runtime diagnostic channel.
///
/// ```compile_fail,E0451
/// use carina_plugin_sdk::protected_material::RegisteredRequest;
/// let _ = RegisteredRequest { request: todo!() };
/// ```
pub struct RegisteredRequest {
    pub(crate) request: http::Request<Vec<u8>>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) diagnostics: ProviderDiagnostics,
}
impl std::fmt::Debug for RegisteredRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredRequest")
            .field("body_bytes", &self.request.body().len())
            .finish()
    }
}
impl ProviderDiagnostics {
    pub(crate) fn prepare_request(&self, request: http::Request<Vec<u8>>) -> RegisteredRequest {
        RegisteredRequest {
            request,
            #[cfg(target_arch = "wasm32")]
            diagnostics: self.clone(),
        }
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
        Ok(self.diagnostics.prepare_request(request))
    }
}
