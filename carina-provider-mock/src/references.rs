//! Table-driven reference resolver shared by native and WASM mock providers.
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, TypeIdentity, ValidatedResourceSchema,
};
use carina_plugin_sdk::types::{
    ProviderError, ProviderErrorKind, ReferenceError, ReferenceOutcome, ReferenceRequest,
};
use std::collections::BTreeMap;

pub type ReferenceTable = BTreeMap<String, Result<ReferenceOutcome, ReferenceError>>;

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

pub fn error(message: impl Into<String>) -> Box<ProviderError> {
    Box::new(ProviderError {
        kind: ProviderErrorKind::InvalidInput,
        message: message.into(),
        resource_id: None,
        cause: None,
        provider_name: Some("mock".into()),
        operation: Some("resolve_references".into()),
        status: None,
        code: Some("MockReferenceError".into()),
        request_id: Some("mock-reference-request".into()),
    })
}

pub fn resolve(
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
            table.get(&value).cloned().unwrap_or_else(|| {
                Ok(ReferenceOutcome::NotFound(error(format!(
                    "reference {value:?} not found"
                ))))
            })
        })
        .collect()
}
