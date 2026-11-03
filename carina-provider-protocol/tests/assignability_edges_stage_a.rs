//! Required wire surface for carina#4233 stage A. No core API is needed to
//! demonstrate the missing-field and protocol-version failures on main.

use carina_provider_protocol::types::ProviderSchemaEnvelope;

#[test]
fn assignability_edges_constructor_rejects_self_edges() {
    assert!(
        carina_provider_protocol::types::AssignabilityEdgeDeclaration::new(
            "semantic.Bucket.Name",
            "semantic.Bucket.Name",
        )
        .is_err()
    );
}

#[test]
fn assignability_edges_deserialization_cannot_bypass_constructor_validation() {
    assert!(
        serde_json::from_value::<ProviderSchemaEnvelope>(serde_json::json!({
            "resources": [],
            "reference_forms": [],
            "assignability_edges": [{
                "source": "semantic.Bucket.Name",
                "sink": "semantic.Bucket.Name",
            }],
        }))
        .is_err()
    );
}

#[test]
fn assignability_edges_field_is_required_even_when_empty() {
    let error = serde_json::from_value::<ProviderSchemaEnvelope>(serde_json::json!({
        "resources": [],
        "reference_forms": [],
    }))
    .expect_err("omitting assignability_edges must not silently mean no edges");
    assert!(error.to_string().contains("assignability_edges"), "{error}");
}

#[test]
fn assignability_edges_wire_json_round_trip_retains_declarations() {
    let json = serde_json::json!({
        "resources": [],
        "reference_forms": [],
        "assignability_edges": [
            {"source": "semantic.Bucket.NewName", "sink": "semantic.Bucket.Name"},
            {"source": "semantic.Bucket.Name", "sink": "semantic.Bucket.Reference"},
        ],
    });
    let envelope: ProviderSchemaEnvelope = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(envelope).unwrap(), json);
}

#[test]
fn assignability_edges_protocol_version_is_21() {
    assert_eq!(carina_provider_protocol::PROTOCOL_VERSION, 21);
}

#[test]
fn assignability_edges_min_supported_protocol_version_is_21() {
    assert_eq!(carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION, 21);
}
