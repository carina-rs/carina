use carina_core::protocol_convert::protocol_to_provider_schema_witness;
use carina_core::schema::{
    AttributeSchema, AttributeType, CreationInputPath, CreationPredicate, CreationRule,
    CreationRuleCode, ReferenceForms, ResourceSchema, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness,
};
use carina_plugin_sdk::provider_schema_to_json;

#[test]
fn guest_schema_projection_preserves_empty_and_nonempty_creation_declarations() {
    let constrained = ResourceSchema::new("test.Constrained")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .creation_constraint(CreationRule::new(
            CreationInputPath::new("name").unwrap(),
            CreationRuleCode::new("test.not_ipv4").unwrap(),
            CreationPredicate::NotIpv4,
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
        value["resources"][0]["creation_constraints"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        value["resources"][1]["creation_constraints"],
        serde_json::json!([])
    );
    let protocol = serde_json::from_str(&json).unwrap();
    let decoded =
        protocol_to_provider_schema_witness(&protocol, ValidatedConfigSchema::default()).unwrap();
    assert_eq!(
        witness.resources()[0].parts().creation_constraints,
        decoded.resources()[0].parts().creation_constraints
    );
}
