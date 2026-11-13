use super::*;
use crate::schema::{AttributeType, StringIdentityLayer, StringMemberConstraints, StructField};

fn catalog() -> crate::parser::ProviderCustomTypes {
    crate::parser::ProviderCustomTypes::loaded(Default::default(), Box::new(|_, _| Ok(())))
}

fn string(identity: &str) -> AttributeType {
    AttributeType::refined_string(Some(TypeIdentity::from_dotted(identity)), None, None, None)
}

fn reference(identity: &str) -> AttributeType {
    AttributeType::reference(TypeIdentity::from_dotted(identity), None, None)
}

fn assert_assignment(source: &AttributeType, target: &AttributeType, accepted: bool) {
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let sink =
        TypeInSchema::schemaless(target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    let source = TypeInSchema::schemaless(source);
    assert_eq!(
        source.assignment_to(sink),
        if accepted {
            TypeAssignability::Assignable
        } else {
            TypeAssignability::Mismatch
        },
        "{} -> {}",
        source.resolved_type_name(),
        target.type_name(),
    );
    let verdict = source.assignment_on_paths(
        sink,
        ReferenceAssignmentCheck::Full,
        &mut Vec::new(),
        &mut Vec::new(),
    );
    assert_eq!(
        !matches!(verdict, AssignmentRelation::Mismatch),
        accepted,
        "{} -> {}: {verdict:?}",
        source.resolved_type_name(),
        target.type_name(),
    );
}

#[test]
fn issue_4384_reference_to_identified_string_requires_identity_admission() {
    assert_assignment(
        &reference("mock.keys.Key.Reference"),
        &string("mock.RecoveryProbe"),
        false,
    );
    assert_assignment(
        &reference("mock.keys.Key.Reference"),
        &string("mock.keys.Key.Reference"),
        true,
    );
    assert_assignment(
        &reference("mock.keys.Key.Reference"),
        &string("mock.Reference"),
        true,
    );
    assert_assignment(
        &reference("mock.Reference"),
        &string("mock.keys.Key.Reference"),
        false,
    );
    assert_assignment(
        &reference("other.Reference"),
        &string("mock.Reference"),
        false,
    );
}

#[test]
fn issue_4384_multi_owner_string_annotation_treats_reference_and_string_sources_equally() {
    use crate::parser::{ResolvedSchemaType, TypeExpr};
    use crate::schema::CompiledAttributeType;

    let identity = TypeIdentity::from_dotted("mock.Reference");
    let broad_length = Some((Some(1), Some(32)));
    let source_string =
        CompiledAttributeType::refined_string(Some(identity.clone()), None, broad_length, None);
    let source_reference = CompiledAttributeType::reference(identity.clone(), None, broad_length);
    let narrower =
        CompiledAttributeType::refined_string(Some(identity), None, Some((Some(1), Some(8))), None);
    let first = ResolvedSchemaType::from_annotation_declaration("a", &source_string).unwrap();
    let second = ResolvedSchemaType::from_annotation_declaration("b", &narrower).unwrap();
    let owned = first.conjoin(&second);
    assert_eq!(owned.owners().iter().len(), 2);
    let target = crate::validation::lift_type_expr(&TypeExpr::SchemaType { owned }).unwrap();
    let AttrTypeKind::String { identity_layer, .. } = target.kind() else {
        panic!("the conjoined annotation must remain a String");
    };
    assert_eq!(identity_layer.parts().length, Some((Some(1), Some(8))));
    let catalog = catalog();
    let sink = target.assignment_sink(&catalog);

    // Declaration compatibility retains identity-bound containment, even for
    // equal identities; the broad source is not contained by the conjoined sink.
    let string_declaration = TypeInSchema::schemaless(&source_string).assignment_to(sink);
    assert_eq!(string_declaration, TypeAssignability::Mismatch);
    assert_eq!(
        TypeInSchema::schemaless(&source_reference).assignment_to(sink),
        string_declaration
    );

    // Same-identity reference admission instead retains the destination check
    // for these overlapping bounds, regardless of the source's scalar form.
    for source in [&source_string, &source_reference] {
        assert!(matches!(
            TypeInSchema::schemaless(source).assignment_on_paths(
                sink,
                ReferenceAssignmentCheck::Full,
                &mut Vec::new(),
                &mut Vec::new(),
            ),
            AssignmentRelation::NeedsLocal(None)
        ));
    }
}

#[test]
fn issue_4384_original_reference_identity_evidence_is_rechecked() {
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let source = reference("mock.keys.Key.Reference");
    let target = string("mock.RecoveryProbe");
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    let source = TypeInSchema::schemaless(&source);
    assert_eq!(
        source.identity_assignment_to(sink),
        TypeAssignability::Mismatch
    );
    assert!(matches!(
        source.assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::IdentifiedLeaves,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        AssignmentRelation::Mismatch
    ));
}

#[test]
fn issue_4384_reference_wire_scalar_flows_stay_accepted() {
    let source = reference("mock.keys.Key.Reference");
    for target in [
        AttributeType::string(),
        AttributeType::refined_string(None, None, Some((Some(1), Some(8))), None),
        reference("mock.OtherReference"),
    ] {
        assert_assignment(&source, &target, true);
    }
    assert_assignment(&AttributeType::string(), &reference("mock.Reference"), true);
    assert_assignment(&string("mock.Other"), &reference("mock.Reference"), true);
    let source = AttributeType::reference(
        TypeIdentity::from_dotted("mock.Reference"),
        None,
        Some((Some(20), Some(30))),
    );
    let target = AttributeType::refined_string(None, None, Some((Some(1), Some(8))), None);
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    assert!(matches!(
        TypeInSchema::schemaless(&source).assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        AssignmentRelation::Assignable
    ));
}

#[test]
fn issue_4384_reference_identity_widening_requires_identity_bound_containment() {
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let target = AttributeType::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Reference")),
            None,
            Some((Some(1), Some(8))),
            None,
        ),
        StringMemberConstraints::new(None, Some((Some(2), Some(4)))),
    );
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    for (length, accepted) in [
        (None, false),
        (Some((Some(1), Some(16))), false),
        (Some((Some(1), Some(8))), true),
        (Some((Some(3), Some(6))), true),
        (Some((Some(10), Some(16))), false),
    ] {
        let source = AttributeType::reference(
            TypeIdentity::from_dotted("mock.keys.Key.Reference"),
            None,
            length,
        );
        let source = TypeInSchema::schemaless(&source);
        assert_eq!(
            source.assignment_to(sink).require_loaded().unwrap(),
            accepted,
            "{length:?}"
        );
        let reference = source.assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(
            matches!(
                (reference, accepted),
                (AssignmentRelation::NeedsLocal(_), true) | (AssignmentRelation::Mismatch, false)
            ),
            "{length:?}"
        );
    }
}

#[test]
fn issue_4384_reference_same_identity_defers_overlapping_destination_members() {
    let target = AttributeType::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("mock.Reference")),
            None,
            None,
            None,
        ),
        StringMemberConstraints::new(None, Some((Some(2), Some(4)))),
    );
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    let source = reference("mock.Reference");
    assert_eq!(
        TypeInSchema::schemaless(&source).assignment_to(sink),
        TypeAssignability::Assignable
    );
    assert!(matches!(
        TypeInSchema::schemaless(&source).assignment_on_paths(
            sink,
            ReferenceAssignmentCheck::Full,
            &mut Vec::new(),
            &mut Vec::new()
        ),
        AssignmentRelation::NeedsLocal(None)
    ));
}

#[test]
fn issue_4384_reference_identity_check_recurses_through_containers_and_defs() {
    let source = reference("mock.keys.Key.Reference");
    let target = string("mock.RecoveryProbe");
    let record = |field| {
        AttributeType::struct_(
            "Record",
            vec![StructField::required("field", field).build()],
        )
    };
    for (source, target) in [
        (
            AttributeType::list(source.clone()),
            AttributeType::list(target.clone()),
        ),
        (
            AttributeType::map(source.clone()),
            AttributeType::map(target.clone()),
        ),
        (record(source.clone()), record(target.clone())),
        (record(source.clone()), AttributeType::map(target.clone())),
        (
            AttributeType::list(source.clone()),
            AttributeType::union(vec![
                AttributeType::bool(),
                AttributeType::list(target.clone()),
            ]),
        ),
    ] {
        assert_assignment(&source, &target, false);
    }
    let source_defs = std::collections::BTreeMap::from([("Leaf".into(), source)]);
    let target_defs = std::collections::BTreeMap::from([("Leaf".into(), target)]);
    let catalog = catalog();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let leaf = AttributeType::ref_("Leaf");
    let sink = leaf
        .in_schema(&target_defs)
        .with_authority(&catalog, SinkAuthority::Owner(&owner));
    assert_eq!(
        leaf.in_schema(&source_defs).assignment_to(sink),
        TypeAssignability::Mismatch
    );
}

#[test]
fn issue_4384_reference_identity_mismatch_defers_only_for_unloaded_edge_evidence() {
    let catalog = crate::parser::ProviderCustomTypes::default();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let source = reference("mock.keys.Key.Reference");
    let target = string("mock.RecoveryProbe");
    let sink =
        TypeInSchema::schemaless(&target).with_authority(&catalog, SinkAuthority::Owner(&owner));
    assert!(matches!(
        TypeInSchema::schemaless(&source).assignment_to(sink),
        TypeAssignability::Unchecked(_)
    ));
}

#[test]
fn issue_4384_reference_widening_uses_only_the_sink_owners_edges() {
    use crate::schema::{
        AssignabilityEdge, AttributeSchema, ResourceSchema, SchemaKind, SchemaRegistry,
        ValidatedProviderSchemaWitness,
    };
    let source_id = TypeIdentity::from_dotted("mock.keys.Key.Reference");
    let target_id = TypeIdentity::from_dotted("mock.RecoveryProbe");
    let producer = ResourceSchema::new("Producer")
        .attribute(
            AttributeSchema::optional("reference", reference("mock.keys.Key.Reference")).build(),
        )
        .validate()
        .unwrap();
    let resource = ResourceSchema::new("Boundary")
        .attribute(
            AttributeSchema::optional("source_string", string("mock.keys.Key.Reference")).build(),
        )
        .attribute(AttributeSchema::optional("target", string("mock.RecoveryProbe")).build())
        .validate()
        .unwrap();
    let mut registry = SchemaRegistry::new();
    registry
        .insert_provider_witness(
            "producer",
            &ValidatedProviderSchemaWitness::new(
                vec![producer],
                Default::default(),
                Default::default(),
                vec![],
            )
            .unwrap(),
        )
        .unwrap();
    for (owner, edges) in [
        (
            "mock",
            vec![AssignabilityEdge::new(source_id, target_id).unwrap()],
        ),
        ("other", vec![]),
    ] {
        let witness = ValidatedProviderSchemaWitness::new(
            vec![resource.clone()],
            Default::default(),
            Default::default(),
            edges,
        )
        .unwrap();
        registry.insert_provider_witness(owner, &witness).unwrap();
    }
    let loaded = crate::provider::loaded_provider_snapshot_from_factories(
        Default::default(),
        registry,
        crate::EnvironmentSnapshot::empty(),
    );
    let source_schema = loaded
        .schema_registry()
        .get("producer", "Producer", SchemaKind::Resource)
        .unwrap();
    let source = source_schema
        .as_schema()
        .type_in_schema(source_schema.as_schema().attributes()["reference"].attr_type());
    for (owner, accepted) in [("mock", true), ("other", false)] {
        let schema = loaded
            .schema_registry()
            .get(owner, "Boundary", SchemaKind::Resource)
            .unwrap();
        let sink = schema.assignment_sink(
            schema.as_schema().attributes()["target"].attr_type(),
            &loaded.provider_context().custom_types,
        );
        assert_eq!(
            source.assignment_to(sink).require_loaded().unwrap(),
            accepted,
            "{owner}"
        );
        assert_eq!(
            !matches!(
                source.assignment_on_paths(
                    sink,
                    ReferenceAssignmentCheck::Full,
                    &mut Vec::new(),
                    &mut Vec::new()
                ),
                AssignmentRelation::Mismatch
            ),
            accepted,
            "{owner}"
        );
    }
}
