use carina_provider_protocol::types::{AttributeType, InputRule};
use serde_json::json;

#[test]
fn condition_is_required_even_for_unconditional_rules() {
    let rule = json!({"scope":"every_write", "target":{"kind":"value","path":"name"}, "code":"test.name", "predicate":{"kind":"not_ipv4"}});
    assert!(serde_json::from_value::<InputRule>(rule.clone()).is_err());
    for condition in [
        json!({"kind":"always"}),
        json!({"kind":"present","value":"flag"}),
        json!({"kind":"equals","value":{"path":"flag","literal":{"kind":"bool","value":true}}}),
        json!({"kind":"not","value":{"kind":"present","value":"flag"}}),
    ] {
        let mut value = rule.clone();
        value["condition"] = condition;
        let decoded: InputRule = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
}

#[test]
fn struct_input_rules_are_required_including_when_empty() {
    let value = json!({"type":"struct", "name":"Settings", "fields":[], "field_groups":[]});
    assert!(serde_json::from_value::<AttributeType>(value.clone()).is_err());
    let mut value = value;
    value["input_rules"] = json!([]);
    let decoded: AttributeType = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
}

#[test]
fn conditional_schema_requires_protocol_27() {
    assert_eq!(carina_provider_protocol::PROTOCOL_VERSION, 28);
    assert_eq!(carina_provider_protocol::MIN_SUPPORTED_PROTOCOL_VERSION, 28);
}

#[test]
fn secret_predicate_wire_record_round_trips_with_its_condition() {
    let value = json!({
        "scope": "every_write",
        "target": {"kind": "value", "path": "value"},
        "code": "value.secret",
        "condition": {"kind": "equals", "value": {
            "path": "type", "literal": {"kind": "string", "value": "secure_string"}
        }},
        "predicate": {"kind": "secret"}
    });
    let decoded = serde_json::from_value::<InputRule>(value.clone());
    assert!(
        decoded.is_ok(),
        "secret must be a supported wire predicate: {decoded:?}"
    );
    assert_eq!(serde_json::to_value(decoded.unwrap()).unwrap(), value);
}

#[test]
fn all_literal_kinds_and_general_predicates_round_trip_without_untyped_values() {
    for literal in [
        json!({"kind":"bool","value":false}),
        json!({"kind":"int","value":-7}),
        json!({"kind":"float","value":1.25}),
        json!({"kind":"string","value":"api-value"}),
        json!({"kind":"enum","value":{"identity":{"provider":"mock","segments":["test","Queue"],"kind":"Mode"},"api_value":"perMessageGroupId"}}),
    ] {
        let value = json!({"scope":"every_write","target":{"kind":"value","path":"target"},"code":"test.equals","condition":{"kind":"equals","value":{"path":"guard","literal":literal}},"predicate":{"kind":"equals","value":literal}});
        let decoded: InputRule = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    }
    for literal in [
        json!({"kind":"list","value":[]}),
        json!({"kind":"float","value":"NaN"}),
        json!({"kind":"null"}),
    ] {
        assert!(
            serde_json::from_value::<carina_provider_protocol::types::InputLiteral>(literal)
                .is_err()
        );
    }
}
