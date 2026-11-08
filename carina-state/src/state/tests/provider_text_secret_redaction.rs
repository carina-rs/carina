//! carina#4305: assert against bytes written by the real local backend.

use super::*;
use crate::StateBackend;
use crate::backends::LocalBackend;
use carina_core::provider::{CreateFailureKind, Observation, ProviderError, Sent};

const SECRET: &str = "read-back-private-4305";
const MASK: &str = "(secret)"; // carina-core's SECRET_VALUE_MASK is crate-private.

async fn persisted_read_back_failure(incomplete: bool) -> String {
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schema = PatternResourceSchema::new("test.resource")
        .attribute(PatternAttributeSchema::required("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional("password", PatternAttributeType::string()).build(),
        )
        .attribute(
            PatternAttributeSchema::provider_populated("computed", PatternAttributeType::string())
                .build(),
        )
        .validate()
        .unwrap();
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert("mock", schema.clone());
    let mut parsed = carina_core::parser::parse_file_and_resolve(
        &format!("provider mock {{}}\nlet probe = mock.test.resource {{ name = \"probe\" password = secret(\"{SECRET}\") }}\n"),
        std::path::Path::new("main.crn"), &context,
    ).unwrap().into_expanded(&schemas, &context).unwrap();
    let desired = carina_core::resource::ResolvedResource::new(
        parsed
            .resources
            .remove(0)
            .into_resolved_dependency_phase_for_test(),
    );
    // This is the classified observation produced when create reports an ID
    // and its finishing read-back fails. Keep the wire error round-trip.
    let detail = format!("mock read-back rejected password {SECRET}; public=read-context-4305");
    let error = ProviderError::from_wire(ProviderError::api_error(detail.clone()).to_wire());
    let failed = super::create_attempt_for_test(desired.id.clone())
        .issued(
            AppliedIdentifier::new("mock-id").unwrap(),
            Sent::AllRequestAttributes,
        )
        .identifier_reported(
            Observation::ReadFailed(error),
            ProviderError::api_error("mock create finishing failed"),
        );
    let CreateFailureKind::IdentifierReported { identified, .. } = failed.into_kind() else {
        panic!("read-back failure must retain the created identifier");
    };
    let identified = if incomplete {
        identified.with_partial_read_for_test(carina_core::provider::PartialReadDiagnostic::new(
            carina_core::provider_text::ProviderDiagnostic::message(detail.into()),
            (vec!["computed".into()])
                .try_into()
                .expect("nonempty partial attributes"),
        ))
    } else {
        identified
    };
    let identified =
        identified.with_secrets_for_test(&crate::provider_text_fixtures::context(&[SECRET]));
    let row = resource_state_from_observation_for_test(
        &desired,
        &identified,
        None,
        Some(schema.as_schema()),
        None,
    )
    .expect("failed read-back row must be persistable");
    assert!(
        row.partial_read()
            .unwrap()
            .missing_attributes()
            .contains("computed")
    );
    let mut state = StateFile::new();
    state.upsert_resource(row).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("carina.state.json");
    LocalBackend::with_path(path.clone())
        .write_state(&state)
        .await
        .unwrap();
    let content = std::fs::read_to_string(path).unwrap();
    let json: serde_json::Value = serde_json::from_str(&content).unwrap();
    let reason = &json["resources"][0]["current"]["partial_read"]["pending"]["reason"];
    assert!(
        reason["detail"]
            .as_str()
            .is_some_and(|text| text.contains("mock read-back")),
        "read-back detail was not persisted: {content}"
    );
    assert!(
        content.contains("read-context-4305"),
        "non-secret diagnostic context was lost: {content}"
    );
    content
}

#[tokio::test]
async fn failed_read_back_secret_is_redacted_from_persisted_state_file() {
    let content = persisted_read_back_failure(false).await;
    assert!(
        !content.contains(SECRET),
        "provider secret plaintext leaked into persisted PendingReRead detail: {content}"
    );
    assert!(
        content.contains(MASK),
        "missing {MASK} in persisted read-back detail: {content}"
    );
}

#[tokio::test]
async fn incomplete_read_back_secret_is_redacted_from_persisted_state_file() {
    let content = persisted_read_back_failure(true).await;
    assert!(
        !content.contains(SECRET),
        "provider secret plaintext leaked into persisted partial_read.reason.detail: {content}"
    );
    assert!(
        content.contains(MASK),
        "missing {MASK} in persisted partial-read detail: {content}"
    );
}
