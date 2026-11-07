use std::collections::BTreeMap;

use carina_core::protocol_convert::{attribute_type_to_protocol, protocol_to_config_schema};
use carina_core::schema::{AttributeType, RawShape, Shape, ValidatedConfigSchema};

#[test]
fn map_length_builder_preserves_bounds_and_typed_keys_through_protocol() {
    let length = Some((Some(0), Some(10)));
    let map = AttributeType::refined_map(
        AttributeType::refined_string(
            None,
            Some(carina_core::schema::PatternSpec::new(
                "^key_[0-9]+$",
                carina_core::schema::PatternMatch::Full,
            )),
            None,
            None,
        ),
        AttributeType::string(),
        length,
    );
    let Shape::Map {
        key,
        value,
        constraints,
    } = map.shape_ref_free().unwrap()
    else {
        panic!("expected map shape");
    };
    assert_eq!(constraints.length(), length);
    assert_eq!(key.type_name(), "String(pattern)");
    assert_eq!(value.type_name(), "String");
    assert!(format!("{:?}", map.raw_shape()).contains("MapConstraints"));
    assert!(format!("{:?}", map.shape_ref_free().unwrap()).contains("MapConstraints"));
    assert!(format!("{map:?}").contains("MapConstraints"));
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([("params".into(), map)])).unwrap();
    let original = &schema.parts()["params"];
    let wire = attribute_type_to_protocol(original).unwrap();
    let json = serde_json::to_value(&wire).unwrap();
    assert_eq!(json["length"], serde_json::json!([0, 10]));
    assert_eq!(
        json["key"]["pattern"],
        serde_json::json!({"regex":"^key_[0-9]+$", "mode":{"kind":"full"}})
    );
    let decoded_wire = serde_json::from_value(json).unwrap();
    let decoded =
        protocol_to_config_schema(BTreeMap::from([("params".into(), decoded_wire)])).unwrap();
    let RawShape::Map {
        key,
        value,
        constraints,
    } = decoded.parts()["params"].raw_shape()
    else {
        panic!("expected decoded map shape");
    };
    assert_eq!(constraints.length(), length);
    assert_eq!(key.type_name(), "String(pattern)");
    assert_eq!(value.type_name(), "String");
    assert_eq!(original.type_name(), "Map<String>");
}

#[test]
fn map_length_existing_builders_default_to_unbounded() {
    for map in [
        AttributeType::map(AttributeType::string()),
        AttributeType::map_with_key(AttributeType::string(), AttributeType::string()),
    ] {
        let RawShape::Map {
            key: _key,
            value: _value,
            constraints,
        } = map.raw_shape()
        else {
            panic!("expected map shape");
        };
        assert_eq!(constraints.length(), None);
    }
}
