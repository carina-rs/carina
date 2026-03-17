//! Typed application error for carina-cli

use carina_core::provider::ProviderError;
use carina_state::BackendError;

/// Typed error enum for carina-cli operations
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// State backend errors (lock contention, I/O, serialization, etc.)
    #[error(transparent)]
    Backend(#[from] BackendError),

    /// Provider errors (AWS API failures, timeouts, etc.)
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// Validation errors (schema mismatch, invalid config, etc.)
    #[error("{0}")]
    Validation(String),

    /// Configuration errors (missing attributes, invalid paths, etc.)
    #[error("{0}")]
    Config(String),
}

impl From<String> for AppError {
    fn from(s: String) -> Self {
        AppError::Config(s)
    }
}

impl From<&str> for AppError {
    fn from(s: &str) -> Self {
        AppError::Config(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_backend_error() {
        let backend_err = BackendError::Configuration("missing bucket".to_string());
        let app_err: AppError = backend_err.into();
        assert!(matches!(
            app_err,
            AppError::Backend(BackendError::Configuration(_))
        ));
        assert!(app_err.to_string().contains("missing bucket"));
    }

    #[test]
    fn from_provider_error() {
        let provider_err = ProviderError::new("timeout");
        let app_err: AppError = provider_err.into();
        assert!(matches!(app_err, AppError::Provider(_)));
        assert!(app_err.to_string().contains("timeout"));
    }

    #[test]
    fn validation_error() {
        let app_err = AppError::Validation("invalid region".to_string());
        assert_eq!(app_err.to_string(), "invalid region");
    }

    #[test]
    fn config_error() {
        let app_err = AppError::Config("missing path".to_string());
        assert_eq!(app_err.to_string(), "missing path");
    }

    #[test]
    fn from_backend_locked_error() {
        let locked = BackendError::Locked {
            lock_id: "abc".to_string(),
            who: "user@host".to_string(),
            operation: "apply".to_string(),
        };
        let app_err: AppError = locked.into();
        assert!(matches!(
            app_err,
            AppError::Backend(BackendError::Locked { .. })
        ));
    }

    #[test]
    fn implements_std_error() {
        let app_err = AppError::Validation("test".to_string());
        let _: &dyn std::error::Error = &app_err;
    }
}
