use carina_core::protocol_convert::protocol_to_provider_schema_witness;
use carina_core::schema::{
    AttributeSchema, AttributeType, InputPath, InputPredicate, InputRule, InputRuleCode,
    ReferenceForms, ResourceSchema, ValidatedConfigSchema, ValidatedProviderSchemaWitness,
};
use carina_plugin_sdk::provider_schema_to_json;

#[test]
fn guest_schema_projection_preserves_empty_and_nonempty_creation_declarations() {
    let constrained = ResourceSchema::new("test.Constrained")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .input_rule(InputRule::value(
            carina_core::schema::InputRuleScope::CreateOnly,
            InputPath::new("name").unwrap(),
            InputRuleCode::new("test.not_ipv4").unwrap(),
            InputPredicate::NotIpv4,
        ))
        .validate()
        .unwrap();
    let empty = ResourceSchema::new("test.Empty").validate().unwrap();
    let witness = ValidatedProviderSchemaWitness::new(
        vec![constrained, empty],
        ValidatedConfigSchema::default(),
        ReferenceForms::new(),
        vec![],
    )
    .unwrap();
    // This is the same projection called by the generated schemas() guest export.
    let json = provider_schema_to_json(&witness).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        value["resources"][0]["input_rules"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(value["resources"][1]["input_rules"], serde_json::json!([]));
    let protocol = serde_json::from_str(&json).unwrap();
    let decoded =
        protocol_to_provider_schema_witness(&protocol, ValidatedConfigSchema::default()).unwrap();
    assert_eq!(
        witness.resources()[0].parts().input_rules,
        decoded.resources()[0].parts().input_rules
    );
}

#[test]
fn guest_schema_projection_preserves_every_write_map_key_scope() {
    use carina_core::schema::{InputRuleScope, InputRuleTarget, NonEmptyString};
    let schema = ResourceSchema::new("test.Tagged")
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        )
        .input_rule(InputRule::new(
            InputRuleScope::EveryWrite,
            InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
            InputRuleCode::new("tags.reserved").unwrap(),
            InputPredicate::ForbiddenPrefix {
                prefix: NonEmptyString::new("aws:").unwrap(),
                ignore_ascii_case: true,
            },
        ))
        .validate()
        .unwrap();
    let witness = ValidatedProviderSchemaWitness::new(
        vec![schema],
        Default::default(),
        Default::default(),
        vec![],
    )
    .unwrap();
    let json = provider_schema_to_json(&witness).unwrap();
    let wire: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        wire["resources"][0]["input_rules"][0]["scope"],
        "every_write"
    );
    assert_eq!(
        wire["resources"][0]["input_rules"][0]["target"],
        serde_json::json!({"kind":"map_keys", "path":"tags"})
    );
    let decoded = protocol_to_provider_schema_witness(
        &serde_json::from_str(&json).unwrap(),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        witness.resources()[0].parts().input_rules,
        decoded.resources()[0].parts().input_rules
    );
}
