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
fn input_rules_are_required_even_when_empty() {
    let error = serde_json::from_value::<types::ResourceSchema>(schema()).unwrap_err();
    assert!(error.to_string().contains("input_rules"));
}

#[test]
fn empty_input_rules_survive_json_round_trip() {
    let mut json = schema();
    json["input_rules"] = serde_json::json!([]);
    let schema: types::ResourceSchema = serde_json::from_value(json).unwrap();
    assert_eq!(
        serde_json::to_value(schema).unwrap()["input_rules"],
        serde_json::json!([])
    );
}

#[test]
fn input_rule_protocol_is_current() {
    assert_eq!(PROTOCOL_VERSION, 25);
    assert_eq!(MIN_SUPPORTED_PROTOCOL_VERSION, 25);
}

#[test]
fn input_rules_require_scope_and_typed_target() {
    let rule = serde_json::json!({
        "scope":"every_write", "target":{"kind":"map_keys","path":"tags"},
        "code":"tags.reserved", "predicate":{"kind":"forbidden_prefix","value":{"prefix":"aws:","ignore_ascii_case":true}}
    });
    let decoded: types::InputRule = serde_json::from_value(rule.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), rule);
    for required in ["scope", "target"] {
        let mut invalid = rule.clone();
        invalid.as_object_mut().unwrap().remove(required);
        assert!(
            serde_json::from_value::<types::InputRule>(invalid)
                .unwrap_err()
                .to_string()
                .contains(required)
        );
    }
}
