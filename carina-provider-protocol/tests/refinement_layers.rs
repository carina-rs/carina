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
fn new_layer_records_reject_unknown_constraints() {
    for kind in ["String", "Int"] {
        for layer in ["identity_layer", "member_layer"] {
            let mut value = serde_json::json!({"type":kind,"identity_layer":{},"member_layer":{}});
            value[layer]["unknown_constraint"] = serde_json::json!(true);
            assert!(serde_json::from_value::<types::AttributeType>(value).is_err());
        }
    }
}

#[test]
fn existing_attribute_variants_keep_unknown_field_compatibility() {
    for value in [
        serde_json::json!({"type":"list", "inner":{"type":"Bool"}, "elements":"allow_duplicates", "future":"ignored"}),
        serde_json::json!({"type":"map", "inner":{"type":"Bool"}, "key":{"type":"Bool"}, "future":"ignored"}),
        serde_json::json!({"type":"struct", "name":"Example", "fields":[], "field_groups":[], "future":"ignored"}),
        serde_json::json!({"type":"union", "members":[{"type":"Bool"}], "future":"ignored"}),
        serde_json::json!({"type":"custom", "name":"Example", "base":{"type":"Bool"}, "future":"ignored"}),
        serde_json::json!({"type":"custom_enum", "name":"Example", "namespace":"test", "base":{"type":"Bool"}, "future":"ignored"}),
    ] {
        let result = serde_json::from_value::<types::AttributeType>(value.clone());
        assert!(result.is_ok(), "{value}: {result:?}");
    }
}
