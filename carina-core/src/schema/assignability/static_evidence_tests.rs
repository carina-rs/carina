use super::*;
use crate::schema::{AttributeType, PatternMatch, PatternSpec};

#[test]
fn static_evidence_keeps_nested_literal_sets_proven() {
    let source = AttributeType::union(vec![
        AttributeType::string_literal_set(vec!["dev".into()]),
        AttributeType::string_literal_set(vec!["prod".into()]),
    ]);
    let target = AttributeType::string_literal_set(vec!["dev".into(), "prod".into()]);
    let catalog = crate::parser::ProviderCustomTypes::default();
    let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    let verdict = TypeInSchema::schemaless(&source).reference_relation_to(sink);
    assert!(matches!(
        verdict.for_destination(),
        AssignmentRelation::Assignable
    ));
    assert!(
        TypeInSchema::schemaless(&source)
            .reference_relation_to(sink)
            .type_proven()
    );
}

#[test]
fn static_evidence_preserves_empty_float_mismatch() {
    let id = TypeIdentity::bare("Scalar");
    let source = AttributeType::refined_float(
        Some(id.clone()),
        Some((Some(crate::finite(4.0)), Some(crate::finite(1.0)))),
    );
    let target = AttributeType::refined_float(Some(id), None);
    let catalog = crate::parser::ProviderCustomTypes::default();
    let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    let verdict = TypeInSchema::schemaless(&source).reference_relation_to(sink);
    assert!(matches!(
        verdict.for_destination(),
        AssignmentRelation::Mismatch
    ));
}

#[test]
fn static_evidence_does_not_prove_provider_validator_widening() {
    use crate::schema::{
        AssignabilityEdge, AttributeSchema, ResourceSchema, SchemaRegistry,
        ValidatedProviderSchemaWitness,
    };
    let from = TypeIdentity::from_dotted("p.Source");
    let to = TypeIdentity::from_dotted("p.Target");
    for (source, target, local) in [
        (
            AttributeType::refined_string(Some(from.clone()), None, None, None),
            AttributeType::refined_string_with_validator(
                to.clone(),
                None,
                None,
                crate::schema::legacy_validator(|_| Ok(())),
                None,
            ),
            true,
        ),
        (
            AttributeType::refined_float(Some(from.clone()), None),
            AttributeType::refined_float_with_validator(
                to.clone(),
                None,
                crate::schema::legacy_validator(|_| Ok(())),
            ),
            false,
        ),
    ] {
        let schema = ResourceSchema::new("Values")
            .attribute(AttributeSchema::optional("source", source.clone()).build())
            .attribute(AttributeSchema::optional("target", target.clone()).build())
            .validate()
            .unwrap();
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema],
            Default::default(),
            Default::default(),
            vec![AssignabilityEdge::new(from.clone(), to.clone()).unwrap()],
        )
        .unwrap();
        let mut schemas = SchemaRegistry::new();
        schemas.insert_provider_witness("p", &witness).unwrap();
        let snapshot = crate::test_utils::loaded_fixture_snapshot(schemas);
        let context = snapshot.provider_context().as_provider_context();
        let owner = crate::parser::SchemaTypeOwner::Provider("p".into());
        let defs = std::collections::BTreeMap::new();
        let sink = target
            .in_schema(&defs)
            .with_authority(&context.custom_types, SinkAuthority::Owner(&owner));
        let verdict = source.in_schema(&defs).reference_relation_to(sink);
        let proven = verdict.type_proven();
        assert!(if local {
            matches!(
                verdict.for_destination(),
                AssignmentRelation::NeedsLocal(None)
            )
        } else {
            matches!(verdict.for_destination(), AssignmentRelation::Assignable)
        });
        assert!(!proven);
        let parsed = crate::parser::ParsedFile::default();
        let bindings = crate::binding_index::BindingIndex::from_parsed(
            &parsed,
            snapshot.schema_registry(),
            context,
        );
        let source = crate::parser::parse_type_expr_str("p.Source", context).unwrap();
        let target = crate::parser::parse_type_expr_str("p.Target", context).unwrap();
        let verdict = crate::validation::admission::static_judgement::judge_static_assignment(
            crate::validation::admission::static_judgement::StaticAssignmentSource::Expression(
                &source,
            ),
            &target,
            &bindings,
            &|_| false,
        );
        assert!(matches!(verdict, Ok(crate::validation::admission::static_judgement::StaticAssignmentVerdict::NeedsLocal)), "{verdict:?}");
    }
}

#[test]
fn static_evidence_proves_loaded_nominal_annotation() {
    use crate::validation::admission::static_judgement::{
        StaticAssignmentSource, StaticAssignmentVerdict, judge_static_assignment,
    };
    let schema = crate::schema::ResourceSchema::new("Thing")
        .attribute(
            crate::schema::AttributeSchema::provider_populated(
                "typed_name",
                AttributeType::ref_("Name"),
            )
            .build(),
        )
        .with_def(
            "Name",
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("test.Name")),
                Some(PatternSpec::new("allowed", PatternMatch::Full)),
                None,
                None,
            ),
        )
        .validate()
        .unwrap();
    let mut schemas = crate::schema::SchemaRegistry::new();
    schemas.insert("test", schema);
    let snapshot = crate::test_utils::loaded_fixture_snapshot(schemas);
    let config = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse("let src = test.Thing {}", config).unwrap();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        snapshot.schema_registry(),
        config,
    );
    let ty = crate::parser::parse_type_expr_str("test.Name", config).unwrap();
    let resolved = crate::validation::resolve_type_expr(&ty, config).unwrap();
    let lifted = crate::validation::lift_type_expr(&resolved).unwrap();
    let result = judge_static_assignment(
        StaticAssignmentSource::Expression(&resolved),
        &ty,
        &bindings,
        &|_| false,
    );
    assert!(
        matches!(result, Ok(StaticAssignmentVerdict::Proven)),
        "{result:?}: {:?}",
        *lifted
    );
    let path = crate::resource::AuthoredAccessPath::new("src", "typed_name");
    let crate::binding_index::RefType::Typed(reference) = bindings.ref_type(&path) else {
        panic!("typed reference")
    };
    let result = judge_static_assignment(
        StaticAssignmentSource::Reference(&reference),
        &ty,
        &bindings,
        &|_| false,
    );
    assert!(
        matches!(result, Ok(StaticAssignmentVerdict::Proven)),
        "{result:?}: {reference:?}"
    );
}

#[test]
fn static_evidence_preserves_destination_obligations() {
    let catalog = crate::parser::ProviderCustomTypes::default();
    let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
    let defs = std::collections::BTreeMap::new();
    let name = AttributeType::refined_string(
        Some(TypeIdentity::from_dotted("test.Name")),
        Some(PatternSpec::new("allowed", PatternMatch::Full)),
        None,
        None,
    );
    let sink = name
        .in_schema(&defs)
        .with_authority(&catalog, SinkAuthority::Owner(&owner));
    let verdict = name.in_schema(&defs).reference_relation_to(sink);
    assert!(verdict.type_proven());
    assert!(matches!(
        verdict.for_destination(),
        AssignmentRelation::NeedsLocal(None)
    ));
    for (source, target) in [
        (
            AttributeType::string(),
            AttributeType::string_literal_set(vec!["dev".into(), "prod".into()]),
        ),
        (
            AttributeType::list(AttributeType::string()),
            AttributeType::list(AttributeType::string_literal_set(vec!["dev".into()])),
        ),
    ] {
        let sink = target
            .in_schema(&defs)
            .with_authority(&catalog, SinkAuthority::Owner(&owner));
        let verdict = source.in_schema(&defs).reference_relation_to(sink);
        assert!(!verdict.type_proven());
        assert!(matches!(
            verdict.for_destination(),
            AssignmentRelation::Assignable
        ));
    }
}

#[test]
fn static_evidence_does_not_prove_overlapping_identity_bounds() {
    let catalog = crate::parser::ProviderCustomTypes::default();
    let owner = crate::parser::SchemaTypeOwner::Provider("test".into());
    let defs = std::collections::BTreeMap::new();
    let name = |maximum| {
        AttributeType::refined_string(
            Some(TypeIdentity::from_dotted("test.Name")),
            None,
            Some((None, Some(maximum))),
            None,
        )
    };
    let source = name(64);
    let target = name(36);
    let sink = target
        .in_schema(&defs)
        .with_authority(&catalog, SinkAuthority::Owner(&owner));
    let verdict = source.in_schema(&defs).reference_relation_to(sink);
    assert!(!verdict.type_proven());
    assert!(matches!(
        verdict.for_destination(),
        AssignmentRelation::NeedsLocal(None)
    ));
}
