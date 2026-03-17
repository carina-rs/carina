//! Typed application errors for carina-cli
//!
//! Replaces `Result<_, String>` with a structured enum so that error context
//! (backend vs provider vs validation vs configuration vs I/O) is preserved.

use carina_core::provider::ProviderError;
use carina_state::BackendError;

/// Top-level error type used by CLI commands and wiring functions.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// State backend error (lock contention, I/O, serialization, etc.)
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// Provider error (AWS API failures, timeouts, etc.)
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Resource or configuration validation error
    #[error("validation: {0}")]
    Validation(String),

    /// CLI or wiring configuration error
    #[error("configuration: {0}")]
    Config(String),

    /// File I/O error
    #[error("I/O: {0}")]
    Io(String),

    /// Serialization / deserialization error
    #[error("serialization: {0}")]
    Serialization(String),
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::Serialization(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_error_from_backend_error_preserves_variant() {
        let backend_err = BackendError::Configuration("bad config".to_string());
        let app_err = AppError::from(backend_err);
        assert!(
            matches!(app_err, AppError::Backend(BackendError::Configuration(ref s)) if s == "bad config")
        );
    }

    #[test]
    fn app_error_from_provider_error_preserves_message() {
        let provider_err = ProviderError::new("timeout reading resource");
        let app_err = AppError::from(provider_err);
        match &app_err {
            AppError::Provider(e) => assert_eq!(e.message, "timeout reading resource"),
            other => panic!("expected Provider variant, got {:?}", other),
        }
    }

    #[test]
    fn app_error_validation_display() {
        let err = AppError::Validation("missing required field".to_string());
        assert_eq!(err.to_string(), "validation: missing required field");
    }

    #[test]
    fn app_error_config_display() {
        let err = AppError::Config("unknown backend type".to_string());
        assert_eq!(err.to_string(), "configuration: unknown backend type");
    }

    #[test]
    fn app_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let app_err = AppError::from(io_err);
        assert!(matches!(app_err, AppError::Io(ref s) if s.contains("file not found")));
    }

    #[test]
    fn app_error_from_serde_error() {
        let serde_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let app_err = AppError::from(serde_err);
        assert!(matches!(app_err, AppError::Serialization(_)));
    }

    /// Verify that wiring functions return AppError, not String.
    /// This is a compile-time check: if the return type is still String, this won't compile.
    #[test]
    fn validate_resources_returns_app_error() {
        let resources = vec![];
        let result: Result<(), AppError> = crate::wiring::validate_resources(&resources);
        assert!(result.is_ok());
    }
}
