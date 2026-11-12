use std::collections::BTreeMap;

use carina_core::schema::{
    AttributeSchema, AttributeType, IdentifiedDeclarationKind, PatternMatch, PatternSpec,
    ProviderSchemaValidationError, ReferenceForm, ReferenceTemplate, ResourceSchema, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness,
};

fn enum_x() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::bare("X"),
        Some(vec!["a".into()]),
        vec![],
        None,
        None,
    )
}

fn string_x(pattern: &str) -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::bare("X")),
        Some(PatternSpec::new(pattern, PatternMatch::Full)),
        None,
        None,
    )
}

#[test]
fn enum_occurrence_does_not_mask_single_schema_scalar_agreement() {
    let declarations = BTreeMap::from([
        ("a_enum".into(), enum_x()),
        ("b_first_string".into(), string_x("[a-z]+")),
        ("c_second_string".into(), string_x("[A-Z]+")),
    ]);
    let error = ValidatedConfigSchema::validate(declarations.clone())
        .expect_err("an enum must not mask disagreement between later String declarations");
    let disagreement = "c_second_string: custom type `X` has conflicting identity-layer constraints at b_first_string and c_second_string";
    assert_eq!(
        error.to_string(),
        format!("provider config schema is invalid: {disagreement}")
    );

    let resource = declarations.into_iter().fold(
        ResourceSchema::new("test.Shapes"),
        |schema, (name, declaration)| {
            schema.attribute(AttributeSchema::optional(name, declaration).build())
        },
    );
    let error = resource
        .validate()
        .expect_err("resource declarations must agree too");
    assert_eq!(
        error.to_string(),
        format!("resource schema `test.Shapes` is invalid: {disagreement}")
    );
}

#[test]
fn enum_occurrence_does_not_mask_aggregate_scalar_agreement() {
    let resources = [enum_x(), string_x("[a-z]+")]
        .into_iter()
        .enumerate()
        .map(|(index, declaration)| {
            ResourceSchema::new(format!("test.Shape{index}"))
                .attribute(AttributeSchema::optional("value", declaration).build())
                .validate()
                .unwrap()
        })
        .collect();
    let config =
        ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), string_x("[A-Z]+"))]))
            .unwrap();
    assert!(matches!(
        ValidatedProviderSchemaWitness::new(resources, config, BTreeMap::new(), vec![]),
        Err(ProviderSchemaValidationError::ConflictingIdentityDeclarations { identity })
            if identity == TypeIdentity::bare("X")
    ));
}

#[test]
fn enum_reference_mixing_defers_reference_form_checks_to_aggregate_admission() {
    let identity = TypeIdentity::bare("X");
    let config = ValidatedConfigSchema::validate(BTreeMap::from([
        ("enum".into(), enum_x()),
        (
            "reference".into(),
            AttributeType::reference(identity.clone(), None, None),
        ),
    ]))
    .unwrap();
    assert!(
        ValidatedProviderSchemaWitness::new(vec![], config.clone(), BTreeMap::new(), vec![])
            .is_ok()
    );
    let forms = BTreeMap::from([(
        identity.clone(),
        ReferenceForm::Template {
            long: ReferenceTemplate::new("prefix/{name}").unwrap(),
        },
    )]);
    assert!(matches!(
        ValidatedProviderSchemaWitness::new(vec![], config, forms, vec![]),
        Err(ProviderSchemaValidationError::NonStringReferenceForm {
            identity: rejected,
            declaration: IdentifiedDeclarationKind::Enum,
        }) if rejected == identity
    ));
}
