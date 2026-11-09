use carina_provider_protocol::{MIN_SUPPORTED_PROTOCOL_VERSION, PROTOCOL_VERSION, types};

#[test]
fn refinement_layers_require_protocol_26() {
    assert_eq!(PROTOCOL_VERSION, 26);
    assert_eq!(MIN_SUPPORTED_PROTOCOL_VERSION, 26);
}

#[test]
fn string_layers_survive_json_round_trip() {
    let value = serde_json::json!({
        "type": "String",
        "identity_layer": {
            "identity": "test.Name",
            "pattern": {"regex":"[a-z]+", "mode":{"kind":"full"}},
            "length": [1, 32]
        },
        "member_layer": {
            "pattern": {"regex":"a.*", "mode":{"kind":"full"}},
            "length": [2, 16]
        }
    });
    let decoded: types::AttributeType = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn both_layer_records_are_required() {
    for kind in ["String", "Int"] {
        for missing in ["identity_layer", "member_layer"] {
            let mut value = serde_json::json!({
                "type": kind,
                "identity_layer": {},
                "member_layer": {}
            });
            value.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<types::AttributeType>(value).is_err());
        }
    }
}

#[test]
fn int_layers_survive_json_round_trip() {
    let value = serde_json::json!({
        "type": "Int",
        "identity_layer": {"identity": "test.Count", "range": [1, 100]},
        "member_layer": {"range": [10, 20]}
    });
    let decoded: types::AttributeType = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn member_layers_reject_identity_and_callback_fields() {
    for field in ["identity", "validate", "to_dsl"] {
        let mut value =
            serde_json::json!({"type":"String", "identity_layer": {}, "member_layer": {}});
        value["member_layer"][field] = serde_json::json!("cannot override the identity");
        assert!(serde_json::from_value::<types::AttributeType>(value).is_err());
    }
}

#[test]
fn flat_constraints_cannot_be_silently_ignored_alongside_layers() {
    for kind in ["String", "Int"] {
        let value = serde_json::json!({"type": kind, "identity_layer": {}, "member_layer": {}, "pattern": {"regex":"x", "mode":{"kind":"full"}}});
        assert!(serde_json::from_value::<types::AttributeType>(value).is_err());
    }
}
