//! Table-driven reference resolver shared by native and WASM mock providers.
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, ValidatedResourceSchema,
};
use carina_provider_protocol::types::{
    ProviderError as ConfiguredError, ReferenceError as ConfiguredReferenceError,
    ReferenceOutcome as ConfiguredOutcome, ReferenceRequest,
};
use std::collections::BTreeMap;

use carina_core::provider::{ProviderError, ProviderErrorKind};
use carina_core::provider_text::{ProviderDiagnostic, ProviderText};
use carina_plugin_sdk::types::{ReferenceError, ReferenceOutcome};

// Textual scenario input, distinct from the errors authored by the provider.
pub type ReferenceTable = BTreeMap<String, Result<ConfiguredOutcome, ConfiguredReferenceError>>;

pub fn identity() -> TypeIdentity {
    TypeIdentity::new(Some("mock"), ["keys", "Key"], "Reference")
}

pub fn resource_schema() -> ValidatedResourceSchema {
    ResourceSchema::new("test.reference")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional("key", AttributeType::reference(identity(), None, None))
                .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "fixed_key",
                AttributeType::reference(identity(), None, None),
            )
            .create_only()
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "secret_key",
                AttributeType::reference(identity(), None, None),
            )
            .write_only()
            .build(),
        )
        .validate()
        .expect("mock reference schema")
}

fn error(message: impl Into<String>) -> Box<ProviderError> {
    Box::new(
        ProviderError::invalid_input(ProviderText::new(message))
            .for_provider("mock")
            .with_operation("resolve_references")
            .with_code("MockReferenceError")
            .with_request_id("mock-reference-request"),
    )
}

/// Author the configured failure using core's composition API. The table is
/// scenario data, not a protocol decoder exposed to provider authors.
fn configured_error(input: ConfiguredError) -> Box<ProviderError> {
    #[derive(Debug)]
    struct ConfiguredReferenceCause(String);
    impl std::fmt::Display for ConfiguredReferenceCause {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for ConfiguredReferenceCause {}
    let kind = match input.kind {
        carina_provider_protocol::types::ProviderErrorKind::InvalidInput => {
            ProviderErrorKind::InvalidInput
        }
        carina_provider_protocol::types::ProviderErrorKind::ApiError => ProviderErrorKind::ApiError,
        carina_provider_protocol::types::ProviderErrorKind::NotFound => ProviderErrorKind::NotFound,
        carina_provider_protocol::types::ProviderErrorKind::Timeout => ProviderErrorKind::Timeout,
        carina_provider_protocol::types::ProviderErrorKind::Internal => ProviderErrorKind::Internal,
    };
    let mut authored = match input.cause {
        Some(cause) => {
            let context = ProviderText::new(input.message);
            let cause = ConfiguredReferenceCause(cause);
            match kind {
                ProviderErrorKind::InvalidInput => {
                    ProviderError::invalid_input_caused_by(context, cause)
                }
                ProviderErrorKind::ApiError => ProviderError::api_error_caused_by(context, cause),
                ProviderErrorKind::NotFound => ProviderError::not_found_caused_by(context, cause),
                ProviderErrorKind::Timeout => ProviderError::timeout_caused_by(context, cause),
                ProviderErrorKind::Internal => ProviderError::internal_caused_by(context, cause),
            }
        }
        None => {
            ProviderError::from_diagnostic(kind, ProviderDiagnostic::message(input.message.into()))
        }
    }
    .with_retryability(input.retryability.into());
    if let Some(resource_id) = input.resource_id {
        authored = authored.for_resource(carina_core::protocol_convert::protocol_to_resource_id(
            &resource_id,
        ));
    }
    if let Some(code) = input.code {
        authored = authored.with_code(code);
    }
    let detail = authored.detail_mut();
    detail.provider_name = input.provider_name.map(Into::into);
    detail.operation = input.operation.map(Into::into);
    detail.status = input.status;
    detail.request_id = input.request_id.map(Into::into);
    Box::new(authored)
}

pub fn resolve_authoring(
    table: &ReferenceTable,
    requests: Vec<ReferenceRequest>,
) -> Vec<Result<ReferenceOutcome, ReferenceError>> {
    requests
        .into_iter()
        .map(|request| {
            let ReferenceRequest { ty, value } = request;
            let carina_plugin_sdk::types::TypeIdentity {
                provider,
                segments,
                kind,
            } = ty;
            if provider != "mock" || segments != ["keys", "Key"] || kind != "Reference" {
                return Err(ReferenceError::InvalidReference(error(format!(
                    "unsupported reference identity {provider}.{}.{kind}",
                    segments.join(".")
                ))));
            }
            let Some(configured) = table.get(&value).cloned() else {
                return Ok(ReferenceOutcome::NotFound(error(format!(
                    "reference {value:?} not found"
                ))));
            };
            match configured {
                Ok(ConfiguredOutcome::Resolved(value)) => Ok(ReferenceOutcome::Resolved(value)),
                Ok(ConfiguredOutcome::NotFound(input)) => {
                    Ok(ReferenceOutcome::NotFound(configured_error(*input)))
                }
                Err(ConfiguredReferenceError::AccessDenied(input)) => {
                    Err(ReferenceError::AccessDenied(configured_error(*input)))
                }
                Err(ConfiguredReferenceError::InvalidReference(input)) => {
                    Err(ReferenceError::InvalidReference(configured_error(*input)))
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::provider_text::Retryability;
    use carina_core::test_utils::provider_text::NativeWireFixture;

    #[test]
    fn configured_reference_results_preserve_retryability() {
        for retryability in [
            Retryability::Unclassified,
            Retryability::Transient,
            Retryability::DependencyBlocked,
        ] {
            for cause in [false, true] {
                let error = if cause {
                    ProviderError::api_error_caused_by(
                        "lookup failed",
                        std::io::Error::other("transport diagnostic"),
                    )
                } else {
                    ProviderError::api_error("lookup failed")
                }
                .with_retryability(retryability)
                .to_wire();
                let table = ReferenceTable::from([
                    (
                        "gone".into(),
                        Ok(ConfiguredOutcome::NotFound(Box::new(error.clone()))),
                    ),
                    (
                        "denied".into(),
                        Err(ConfiguredReferenceError::AccessDenied(Box::new(
                            error.clone(),
                        ))),
                    ),
                    (
                        "invalid".into(),
                        Err(ConfiguredReferenceError::InvalidReference(Box::new(error))),
                    ),
                ]);
                // Exercise the JSON consumed by both native and WASM controls.
                let table = serde_json::from_str(&serde_json::to_string(&table).unwrap()).unwrap();
                let requests = ["gone", "denied", "invalid"]
                    .into_iter()
                    .map(|value| ReferenceRequest {
                        ty: carina_plugin_sdk::types::TypeIdentity {
                            provider: "mock".into(),
                            segments: vec!["keys".into(), "Key".into()],
                            kind: "Reference".into(),
                        },
                        value: value.into(),
                    })
                    .collect();
                for result in resolve_authoring(&table, requests) {
                    let error = match result {
                        Ok(ReferenceOutcome::NotFound(error))
                        | Err(ReferenceError::AccessDenied(error))
                        | Err(ReferenceError::InvalidReference(error)) => error,
                        other => panic!("expected a configured failure: {other:?}"),
                    };
                    assert_eq!(error.retryability(), retryability);
                    assert_eq!(error.to_wire().cause.is_some(), cause);
                }
            }
        }
    }

    #[test]
    fn reference_table_rejects_removed_transient_variant() {
        for variant in ["Transient", "transient"] {
            let error = ProviderError::api_error("temporary lookup failure")
                .with_retryability(Retryability::Transient)
                .to_wire();
            let table = serde_json::json!({"retry": {"Err": {(variant): error}}});
            let error = serde_json::from_value::<ReferenceTable>(table).unwrap_err();
            assert!(error.to_string().contains("unknown variant"), "{error}");
        }
    }
}
