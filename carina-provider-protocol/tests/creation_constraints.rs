use carina_provider_protocol::{MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION, types};

fn schema() -> serde_json::Value {
    serde_json::json!({
        "resource_type": "test.Bucket",
        "attributes": {},
        "field_groups": [],
        "default_prevent_destroy": false
    })
}

#[test]
fn creation_constraints_are_required_even_when_empty() {
    let error = serde_json::from_value::<types::ResourceSchema>(schema()).unwrap_err();
    assert!(error.to_string().contains("creation_constraints"));
}

#[test]
fn empty_creation_constraints_survive_json_round_trip() {
    let mut json = schema();
    json["creation_constraints"] = serde_json::json!([]);
    let schema: types::ResourceSchema = serde_json::from_value(json).unwrap();
    assert_eq!(
        serde_json::to_value(schema).unwrap()["creation_constraints"],
        serde_json::json!([])
    );
}

#[test]
fn creation_constraint_protocol_is_exactly_23() {
    assert_eq!(PROTOCOL_VERSION, 23);
    assert_eq!(MIN_SUPPORTED_PROTOCOL_VERSION, 23);
}
