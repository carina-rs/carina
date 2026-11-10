use std::collections::BTreeMap;

use carina_core::protocol_convert::protocol_to_config_schema;
use carina_core::schema::{PatternMatch, PatternReason, PatternSpec, RawShape};
use carina_provider_protocol::types as protocol;

#[test]
fn nested_custom_precedence_preserves_the_complete_mode_and_reason() {
    let terminal = protocol::AttributeType::String {
        identity_layer: protocol::StringIdentityLayer {
            pattern: Some(protocol::PatternSpec::new(
                "base",
                protocol::PatternMatch::Full,
            )),
            length: None,
            validate: None,
            to_dsl: None,
            identity: None,
        },
        member_layer: Default::default(),
    };
    let middle = protocol::AttributeType::Custom {
        name: "ignored.Middle".into(),
        base: Box::new(terminal),
        pattern: Some(protocol::PatternSpec::new(
            "abc",
            protocol::PatternMatch::Substring {
                reason: protocol::PatternReason::new(
                    "The middle declaration permits containing text",
                )
                .unwrap(),
            },
        )),
        length: None,
        validate: None,
        to_dsl: None,
    };
    for (outer, expected) in [
        (
            Some(protocol::PatternSpec::new(
                "abc",
                protocol::PatternMatch::Full,
            )),
            PatternSpec::new("abc", PatternMatch::Full),
        ),
        (
            None,
            PatternSpec::new(
                "abc",
                PatternMatch::Substring {
                    reason: PatternReason::new("The middle declaration permits containing text")
                        .unwrap(),
                },
            ),
        ),
    ] {
        let record = protocol::AttributeType::Custom {
            name: "test.Outer".into(),
            base: Box::new(middle.clone()),
            pattern: outer,
            length: None,
            validate: None,
            to_dsl: None,
        };
        let decoded = serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();
        let schema =
            protocol_to_config_schema(BTreeMap::from([("value".into(), decoded)])).unwrap();
        let RawShape::String {
            identity_layer:
                carina_core::schema::StringIdentityParts {
                    identity,
                    pattern,
                    length: _length,
                    validate: _validate,
                    to_dsl: _to_dsl,
                },
            member_layer: _,
        } = schema.parts()["value"].raw_shape()
        else {
            panic!("String refinement");
        };
        assert_eq!(identity.unwrap().to_string(), "test.Outer");
        assert_eq!(pattern, Some(&expected));
    }
}

#[test]
fn valid_outer_custom_spec_cannot_admit_an_incomplete_inner_pattern() {
    for inner in [
        serde_json::json!("abc"),
        serde_json::json!({"regex":"abc"}),
        serde_json::json!({"regex":"abc","mode":{"kind":"unknown"}}),
        serde_json::json!({"regex":"abc","mode":{"kind":"substring","reason":""}}),
    ] {
        let wire = serde_json::json!({
            "type":"custom", "name":"test.Outer",
            "pattern":{"regex":"abc","mode":{"kind":"full"}},
            "base":{"type":"custom","name":"ignored.Middle", "base":{"type":"String", "identity_layer": {"pattern":inner}, "member_layer": {}}}
        });
        assert!(serde_json::from_value::<protocol::AttributeType>(wire).is_err());
    }
}
