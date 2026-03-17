//! Cloud Control API interaction layer.
//!
//! This module contains low-level methods for interacting with the AWS Cloud Control
//! API: creating, reading, updating, and deleting resources, as well as polling for
//! operation completion and retry/error handling logic.

use std::time::Duration;

use aws_sdk_cloudcontrol::types::OperationStatus;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use carina_core::provider::{ProviderError, ProviderResult};

use super::{
    AwsccProvider, CREATE_RETRY_INITIAL_DELAY_SECS, CREATE_RETRY_MAX_ATTEMPTS,
    CREATE_RETRY_MAX_DELAY_SECS, DELETE_RETRY_INITIAL_DELAY_SECS, DELETE_RETRY_MAX_ATTEMPTS,
    DELETE_RETRY_MAX_DELAY_SECS,
};

impl AwsccProvider {
    /// Get a resource by identifier using Cloud Control API
    pub async fn cc_get_resource(
        &self,
        type_name: &str,
        identifier: &str,
    ) -> ProviderResult<Option<serde_json::Value>> {
        let result = self
            .cloudcontrol_client
            .get_resource()
            .type_name(type_name)
            .identifier(identifier)
            .send()
            .await;

        match result {
            Ok(response) => {
                if let Some(desc) = response.resource_description()
                    && let Some(props_str) = desc.properties()
                {
                    let props = super::parse_resource_properties(props_str)?;
                    Ok(Some(props))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                if Self::is_not_found_error(&e) {
                    Ok(None)
                } else {
                    let detail = Self::format_sdk_error(&e);
                    Err(ProviderError::new(format!(
                        "Failed to get resource: {}",
                        detail
                    )))
                }
            }
        }
    }

    /// Create a resource using Cloud Control API, with retry logic for retryable errors.
    ///
    /// Some operations fail transiently due to eventual consistency in AWS
    /// (e.g., IPAM Pool CIDR propagation delays cause "missing a source resource"
    /// errors when creating subnets). This method retries with exponential backoff
    /// for such errors.
    pub async fn cc_create_resource(
        &self,
        type_name: &str,
        desired_state: serde_json::Value,
    ) -> ProviderResult<String> {
        let mut delay_secs = CREATE_RETRY_INITIAL_DELAY_SECS;

        for attempt in 0..=CREATE_RETRY_MAX_ATTEMPTS {
            let result = self
                .cloudcontrol_client
                .create_resource()
                .type_name(type_name)
                .desired_state(desired_state.to_string())
                .send()
                .await;

            match result {
                Ok(response) => {
                    let request_token =
                        response
                            .progress_event()
                            .and_then(|p| p.request_token())
                            .ok_or_else(|| ProviderError::new("No request token returned"))?;

                    match self.wait_for_operation(request_token).await {
                        Ok(identifier) => return Ok(identifier),
                        Err(e)
                            if Self::is_retryable_status_message(&e.message)
                                && attempt < CREATE_RETRY_MAX_ATTEMPTS =>
                        {
                            log::warn!(
                                "Retryable error creating {} (attempt {}/{}): {}. Retrying in {}s...",
                                type_name,
                                attempt + 1,
                                CREATE_RETRY_MAX_ATTEMPTS,
                                e.message,
                                delay_secs,
                            );
                            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                            delay_secs = (delay_secs * 2).min(CREATE_RETRY_MAX_DELAY_SECS);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    if Self::is_retryable_sdk_error(&e) && attempt < CREATE_RETRY_MAX_ATTEMPTS {
                        log::warn!(
                            "Retryable error creating {} (attempt {}/{}): {:?}. Retrying in {}s...",
                            type_name,
                            attempt + 1,
                            CREATE_RETRY_MAX_ATTEMPTS,
                            e,
                            delay_secs,
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        delay_secs = (delay_secs * 2).min(CREATE_RETRY_MAX_DELAY_SECS);
                        continue;
                    }
                    let detail = Self::format_sdk_error(&e);
                    return Err(ProviderError::new(format!(
                        "Failed to create resource: {}",
                        detail
                    )));
                }
            }
        }

        Err(ProviderError::new(format!(
            "Failed to create resource {} after {} retry attempts",
            type_name, CREATE_RETRY_MAX_ATTEMPTS
        )))
    }

    /// Update a resource using Cloud Control API
    pub async fn cc_update_resource(
        &self,
        type_name: &str,
        identifier: &str,
        patch_ops: Vec<serde_json::Value>,
    ) -> ProviderResult<()> {
        if patch_ops.is_empty() {
            return Ok(());
        }

        let patch_document = serde_json::to_string(&patch_ops)
            .map_err(|e| ProviderError::new("Failed to build patch").with_cause(e))?;

        let result = self
            .cloudcontrol_client
            .update_resource()
            .type_name(type_name)
            .identifier(identifier)
            .patch_document(patch_document)
            .send()
            .await
            .map_err(|e| {
                let detail = Self::format_sdk_error(&e);
                ProviderError::new(format!("Failed to update resource: {}", detail))
            })?;

        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await?;
        }

        Ok(())
    }

    /// Delete a resource using Cloud Control API, with retry logic for retryable errors.
    ///
    /// Uses resource-type-specific polling timeouts. IPAM-related resources
    /// get a longer timeout since their deletion via CloudControl API can
    /// take 15-30 minutes. Retries with exponential backoff on transient errors
    /// such as throttling or service unavailability.
    pub async fn cc_delete_resource(
        &self,
        type_name: &str,
        identifier: &str,
    ) -> ProviderResult<()> {
        let mut delay_secs = DELETE_RETRY_INITIAL_DELAY_SECS;
        let max_polling_attempts = Self::max_polling_attempts(type_name, "delete");

        for attempt in 0..=DELETE_RETRY_MAX_ATTEMPTS {
            let result = self
                .cloudcontrol_client
                .delete_resource()
                .type_name(type_name)
                .identifier(identifier)
                .send()
                .await;

            match result {
                Ok(response) => {
                    if let Some(request_token) =
                        response.progress_event().and_then(|p| p.request_token())
                    {
                        match self
                            .wait_for_operation_with_attempts(request_token, max_polling_attempts)
                            .await
                        {
                            Ok(_) => return Ok(()),
                            Err(e)
                                if Self::is_retryable_status_message(&e.message)
                                    && attempt < DELETE_RETRY_MAX_ATTEMPTS =>
                            {
                                log::warn!(
                                    "Retryable error deleting {} (attempt {}/{}): {}. Retrying in {}s...",
                                    type_name,
                                    attempt + 1,
                                    DELETE_RETRY_MAX_ATTEMPTS,
                                    e.message,
                                    delay_secs,
                                );
                                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                                delay_secs = (delay_secs * 2).min(DELETE_RETRY_MAX_DELAY_SECS);
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    if Self::is_retryable_sdk_error(&e) && attempt < DELETE_RETRY_MAX_ATTEMPTS {
                        log::warn!(
                            "Retryable error deleting {} (attempt {}/{}): {:?}. Retrying in {}s...",
                            type_name,
                            attempt + 1,
                            DELETE_RETRY_MAX_ATTEMPTS,
                            e,
                            delay_secs,
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        delay_secs = (delay_secs * 2).min(DELETE_RETRY_MAX_DELAY_SECS);
                        continue;
                    }
                    let detail = Self::format_sdk_error(&e);
                    return Err(ProviderError::new(format!(
                        "Failed to delete resource: {}",
                        detail
                    )));
                }
            }
        }

        Err(ProviderError::new(format!(
            "Failed to delete resource {} after {} retry attempts",
            type_name, DELETE_RETRY_MAX_ATTEMPTS
        )))
    }

    /// Returns the max polling attempts for a given resource type and operation.
    ///
    /// Some resource types (e.g., IPAM Pool) take significantly longer to delete
    /// via the CloudControl API than the default timeout allows.
    pub(crate) fn max_polling_attempts(type_name: &str, operation: &str) -> u32 {
        // IPAM Pool deletions can take 15-30 minutes via CloudControl API
        if operation == "delete" && (type_name.contains("IPAMPool") || type_name.contains("IPAM")) {
            return 360; // 30 minutes (360 * 5s)
        }
        120 // Default: 10 minutes (120 * 5s)
    }

    /// Formats an SDK error into a human-readable message.
    ///
    /// The `SdkError::Display` implementation only outputs generic labels like "service error"
    /// without including the actual error code or message. This method extracts the structured
    /// error metadata (code and message) from service errors to provide actionable error messages.
    pub(crate) fn format_sdk_error<E, R>(error: &SdkError<E, R>) -> String
    where
        E: ProvideErrorMetadata + std::fmt::Display,
    {
        match error {
            SdkError::ServiceError(service_error) => {
                let err = service_error.err();
                let code = err.code().unwrap_or("Unknown");
                let message = err.message().unwrap_or("no details");
                format!("{}: {}", code, message)
            }
            other => format!("{}", other),
        }
    }

    /// Returns true if the SDK error represents a "not found" condition.
    ///
    /// Uses structured error metadata (`ProvideErrorMetadata::code()`) instead of
    /// fragile string matching against Debug-formatted output.
    ///
    /// Not-found error codes:
    /// - `ResourceNotFoundException`: The resource does not exist
    /// - `HandlerNotFoundException`: The resource handler was not found
    pub(crate) fn is_not_found_error<E, R>(error: &SdkError<E, R>) -> bool
    where
        E: ProvideErrorMetadata,
    {
        const NOT_FOUND_ERROR_CODES: &[&str] =
            &["ResourceNotFoundException", "HandlerNotFoundException"];

        match error {
            SdkError::ServiceError(service_error) => {
                let err = service_error.err();
                if let Some(code) = err.code() {
                    NOT_FOUND_ERROR_CODES.contains(&code)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Returns true if an AWS SDK error represents a retryable condition.
    ///
    /// Uses structured error types from the AWS SDK rather than string matching.
    /// This detects retryable conditions based on the error variant or error code,
    /// which are part of the AWS API contract and more stable than error messages.
    ///
    /// Retryable error types:
    /// - `ThrottlingException`: Request rate exceeded (covers "Throttling", "Rate exceeded")
    /// - `ServiceInternalErrorException`: AWS internal server error
    /// - `HandlerFailureException`: Resource handler failed (may be transient)
    /// - `HandlerInternalFailureException`: Internal handler error
    /// - `NetworkFailureException`: Network connectivity issues
    /// - `ConcurrentOperationException`: Another operation is in progress
    /// - `NotStabilizedException`: Resource not yet stabilized
    /// - `SdkError::TimeoutError`: Connection timeout
    /// - `SdkError::DispatchFailure`: HTTP dispatch failure
    pub(crate) fn is_retryable_sdk_error<E, R>(error: &SdkError<E, R>) -> bool
    where
        E: ProvideErrorMetadata,
    {
        /// Error codes from the CloudControl API that indicate retryable conditions.
        const RETRYABLE_ERROR_CODES: &[&str] = &[
            "ThrottlingException",
            "ServiceInternalErrorException",
            "HandlerFailureException",
            "HandlerInternalFailureException",
            "NetworkFailureException",
            "ConcurrentOperationException",
            "NotStabilizedException",
        ];

        match error {
            SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => true,
            SdkError::ServiceError(service_error) => {
                let err = service_error.err();
                if let Some(code) = err.code() {
                    RETRYABLE_ERROR_CODES.contains(&code)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Returns true if a CloudControl operation status message indicates a retryable condition.
    ///
    /// When a CloudControl operation (create/delete) succeeds at the API level but the
    /// async operation fails, the error details come as a plain-text status message from
    /// `progress_event.status_message()`. These messages don't have structured error codes,
    /// so string pattern matching is the only option.
    ///
    /// **Fragility note**: These patterns depend on AWS error message wording. If AWS
    /// changes the message format, retries may silently stop working. The patterns below
    /// are based on observed CloudControl API behavior as of 2025:
    ///
    /// - `"missing a source resource"`: IPAM Pool CIDR propagation delay causes subnet
    ///   creation to fail transiently while the pool is still provisioning.
    /// - `"Throttling"` / `"Rate exceeded"` / `"RequestLimitExceeded"`: Downstream service
    ///   throttling reported through CloudControl operation status.
    /// - `"ServiceUnavailable"` / `"InternalError"`: Transient downstream service errors
    ///   reported through CloudControl operation status.
    pub(crate) fn is_retryable_status_message(status_message: &str) -> bool {
        const RETRYABLE_STATUS_PATTERNS: &[&str] = &[
            "missing a source resource",
            "Throttling",
            "Rate exceeded",
            "RequestLimitExceeded",
            "ServiceUnavailable",
            "InternalError",
        ];
        RETRYABLE_STATUS_PATTERNS
            .iter()
            .any(|pattern| status_message.contains(pattern))
    }

    /// Wait for a Cloud Control operation to complete
    pub(crate) async fn wait_for_operation(&self, request_token: &str) -> ProviderResult<String> {
        self.wait_for_operation_with_attempts(request_token, 120)
            .await
    }

    /// Wait for a Cloud Control operation to complete with a configurable number of attempts
    async fn wait_for_operation_with_attempts(
        &self,
        request_token: &str,
        max_attempts: u32,
    ) -> ProviderResult<String> {
        let delay = Duration::from_secs(5);

        for _ in 0..max_attempts {
            let status = self
                .cloudcontrol_client
                .get_resource_request_status()
                .request_token(request_token)
                .send()
                .await
                .map_err(|e| ProviderError::new("Failed to get operation status").with_cause(e))?;

            if let Some(progress) = status.progress_event() {
                match progress.operation_status() {
                    Some(OperationStatus::Success) => {
                        return Ok(progress.identifier().unwrap_or("").to_string());
                    }
                    Some(OperationStatus::Failed) => {
                        let msg = progress.status_message().unwrap_or("Unknown error");
                        return Err(ProviderError::new(format!("Operation failed: {}", msg)));
                    }
                    Some(OperationStatus::CancelComplete) => {
                        return Err(ProviderError::new("Operation was cancelled"));
                    }
                    _ => {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(ProviderError::new("Operation timed out").timeout())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // is_retryable_status_message tests
    // =========================================================================

    #[test]
    fn test_is_retryable_status_message_ipam_source_resource() {
        assert!(AwsccProvider::is_retryable_status_message(
            "Operation failed: IpamPool 'ipam-pool-xxx' is missing a source resource"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_throttling() {
        assert!(AwsccProvider::is_retryable_status_message(
            "Throttling: Rate exceeded"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_request_limit() {
        assert!(AwsccProvider::is_retryable_status_message(
            "RequestLimitExceeded: too many requests"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_service_unavailable() {
        assert!(AwsccProvider::is_retryable_status_message(
            "ServiceUnavailable: try again later"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_internal_error() {
        assert!(AwsccProvider::is_retryable_status_message(
            "InternalError: something went wrong"
        ));
    }

    #[test]
    fn test_is_not_retryable_status_message() {
        assert!(!AwsccProvider::is_retryable_status_message(
            "InvalidParameterValue: invalid CIDR"
        ));
        assert!(!AwsccProvider::is_retryable_status_message(
            "ResourceNotFoundException: not found"
        ));
        assert!(!AwsccProvider::is_retryable_status_message(
            "AccessDeniedException: not authorized"
        ));
    }

    // =========================================================================
    // is_retryable_sdk_error tests
    // =========================================================================

    fn error_meta(code: &str) -> aws_smithy_types::error::ErrorMetadata {
        aws_smithy_types::error::ErrorMetadata::builder()
            .code(code)
            .build()
    }

    #[test]
    fn test_is_retryable_sdk_error_throttling() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_service_internal() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ServiceInternalErrorException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ServiceInternalErrorException(
            ServiceInternalErrorException::builder()
                .message("Internal error")
                .meta(error_meta("ServiceInternalErrorException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_handler_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::HandlerFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::HandlerFailureException(
            HandlerFailureException::builder()
                .message("Handler failed")
                .meta(error_meta("HandlerFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_handler_internal_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::HandlerInternalFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::HandlerInternalFailureException(
            HandlerInternalFailureException::builder()
                .message("Internal failure")
                .meta(error_meta("HandlerInternalFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_network_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::NetworkFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::NetworkFailureException(
            NetworkFailureException::builder()
                .message("Network error")
                .meta(error_meta("NetworkFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_concurrent_operation() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ConcurrentOperationException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ConcurrentOperationException(
            ConcurrentOperationException::builder()
                .message("Concurrent operation")
                .meta(error_meta("ConcurrentOperationException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_not_stabilized() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::NotStabilizedException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::NotStabilizedException(
            NotStabilizedException::builder()
                .message("Not stabilized")
                .meta(error_meta("NotStabilizedException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_invalid_request() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::InvalidRequestException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::InvalidRequestException(
            InvalidRequestException::builder()
                .message("Invalid request")
                .meta(error_meta("InvalidRequestException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_already_exists() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::AlreadyExistsException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::AlreadyExistsException(
            AlreadyExistsException::builder()
                .message("Already exists")
                .meta(error_meta("AlreadyExistsException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_timeout() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let sdk_err: SdkError<CreateResourceError, http::Response<&str>> =
            SdkError::timeout_error("connection timed out");
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_delete_throttling() {
        use aws_sdk_cloudcontrol::operation::delete_resource::DeleteResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = DeleteResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_delete_type_not_found() {
        use aws_sdk_cloudcontrol::operation::delete_resource::DeleteResourceError;
        use aws_sdk_cloudcontrol::types::error::TypeNotFoundException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = DeleteResourceError::TypeNotFoundException(
            TypeNotFoundException::builder()
                .message("Type not found")
                .meta(error_meta("TypeNotFoundException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    // =========================================================================
    // is_not_found_error tests
    // =========================================================================

    #[test]
    fn test_is_not_found_error_resource_not_found() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_sdk_cloudcontrol::types::error::ResourceNotFoundException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = GetResourceError::ResourceNotFoundException(
            ResourceNotFoundException::builder()
                .message("Resource not found")
                .meta(error_meta("ResourceNotFoundException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_handler_not_found_code() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = GetResourceError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("HandlerNotFoundException")
                .message("Handler not found")
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_false_for_throttling() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = GetResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_false_for_timeout() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let sdk_err: SdkError<GetResourceError, http::Response<&str>> =
            SdkError::timeout_error("connection timed out");
        assert!(!AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_max_polling_attempts_ipam_pool_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAMPool", "delete"),
            360
        );
    }

    #[test]
    fn test_max_polling_attempts_ipam_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAM", "delete"),
            360
        );
    }

    #[test]
    fn test_max_polling_attempts_default_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::VPC", "delete"),
            120
        );
    }

    #[test]
    fn test_max_polling_attempts_ipam_create() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAMPool", "create"),
            120
        );
    }

    // =========================================================================
    // format_sdk_error tests
    // =========================================================================

    #[test]
    fn test_format_sdk_error_create_resource_error() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::GeneralServiceException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let meta = aws_smithy_types::error::ErrorMetadata::builder()
            .code("GeneralServiceException")
            .message("Handler returned status FAILED")
            .build();
        let err = CreateResourceError::GeneralServiceException(
            GeneralServiceException::builder()
                .message("Handler returned status FAILED")
                .meta(meta)
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        let formatted = AwsccProvider::format_sdk_error(&sdk_err);
        assert_eq!(
            formatted,
            "GeneralServiceException: Handler returned status FAILED"
        );
    }

    #[test]
    fn test_format_sdk_error_delete_resource_error() {
        use aws_sdk_cloudcontrol::operation::delete_resource::DeleteResourceError;
        use aws_sdk_cloudcontrol::types::error::GeneralServiceException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let meta = aws_smithy_types::error::ErrorMetadata::builder()
            .code("GeneralServiceException")
            .message("Handler returned status FAILED")
            .build();
        let err = DeleteResourceError::GeneralServiceException(
            GeneralServiceException::builder()
                .message("Handler returned status FAILED")
                .meta(meta)
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        let formatted = AwsccProvider::format_sdk_error(&sdk_err);
        assert_eq!(
            formatted,
            "GeneralServiceException: Handler returned status FAILED"
        );
    }

    #[test]
    fn test_format_sdk_error_service_error() {
        use aws_sdk_cloudcontrol::operation::update_resource::UpdateResourceError;
        use aws_sdk_cloudcontrol::types::error::GeneralServiceException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let meta = aws_smithy_types::error::ErrorMetadata::builder()
            .code("GeneralServiceException")
            .message("Handler returned status FAILED")
            .build();
        let err = UpdateResourceError::GeneralServiceException(
            GeneralServiceException::builder()
                .message("Handler returned status FAILED")
                .meta(meta)
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        let formatted = AwsccProvider::format_sdk_error(&sdk_err);
        assert_eq!(
            formatted,
            "GeneralServiceException: Handler returned status FAILED"
        );
    }

    #[test]
    fn test_format_sdk_error_get_resource_error() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_sdk_cloudcontrol::types::error::GeneralServiceException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let meta = aws_smithy_types::error::ErrorMetadata::builder()
            .code("GeneralServiceException")
            .message("Handler returned status FAILED")
            .build();
        let err = GetResourceError::GeneralServiceException(
            GeneralServiceException::builder()
                .message("Handler returned status FAILED")
                .meta(meta)
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        let formatted = AwsccProvider::format_sdk_error(&sdk_err);
        assert_eq!(
            formatted,
            "GeneralServiceException: Handler returned status FAILED"
        );
    }

    #[test]
    fn test_format_sdk_error_timeout() {
        use aws_sdk_cloudcontrol::operation::update_resource::UpdateResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let sdk_err: SdkError<UpdateResourceError, http::Response<&str>> =
            SdkError::timeout_error("connection timed out");
        let formatted = AwsccProvider::format_sdk_error(&sdk_err);
        assert_eq!(formatted, "request has timed out");
    }
}
