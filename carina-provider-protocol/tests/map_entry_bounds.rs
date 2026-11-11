use carina_provider_protocol::types::AttributeType;

#[test]
fn map_length_json_preserves_entry_bounds() {
    let json = serde_json::json!({
        "type": "map", "inner": {"type": "String", "identity_layer": {}, "member_layer": {}},
        "key": {"type": "String", "identity_layer": {}, "member_layer": {}}, "length": [0, 10],
    });
    let map: AttributeType = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(map).unwrap(), json);
}

#[test]
fn map_length_json_without_bound_stays_unbounded() {
    let json = serde_json::json!({
        "type": "map", "inner": {"type": "String", "identity_layer": {}, "member_layer": {}}, "key": {"type": "String", "identity_layer": {}, "member_layer": {}},
    });
    let map: AttributeType = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(serde_json::to_value(map).unwrap(), json);
}
