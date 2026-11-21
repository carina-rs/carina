use super::*;
use crate::schema::{AttributeType, PatternMatch, PatternSpec};

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
    assert!(matches!(
        name.in_schema(&defs).static_assignment_to(sink),
        AssignmentRelation::Assignable
    ));
    assert!(matches!(
        name.in_schema(&defs).assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
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
        assert!(matches!(
            source.in_schema(&defs).static_assignment_to(sink),
            AssignmentRelation::NeedsLocal(None)
        ));
        assert!(matches!(
            source.in_schema(&defs).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
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
    assert!(matches!(
        source.in_schema(&defs).static_assignment_to(sink),
        AssignmentRelation::Mismatch
    ));
    assert!(matches!(
        source.in_schema(&defs).assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        ),
        AssignmentRelation::NeedsLocal(None)
    ));
}
