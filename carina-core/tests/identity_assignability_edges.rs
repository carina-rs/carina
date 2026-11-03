#[path = "support/identity_edges.rs"]
pub mod identity_edges;

use carina_core::parser::{ProviderCustomTypes, SchemaTypeOwner, TypeExpr};
use carina_core::schema::{AttributeType, SchemaKind, StructField, TypeInSchema};
use identity_edges::{identity, snapshot, string};

fn schema_fingerprints(edges: Vec<carina_core::schema::AssignabilityEdge>) -> Vec<String> {
    use carina_core::provider::ProviderFactory;
    use carina_core::schema::{SchemaRegistry, ValidatedProviderSchemaWitness};

    let witness = ValidatedProviderSchemaWitness::with_assignability_edges(
        identity_edges::EdgeFactory {
            owner: "mock",
            edges: false,
        }
        .schemas(),
        Default::default(),
        Default::default(),
        edges,
    )
    .unwrap();
    let mut registry = SchemaRegistry::new();
    registry.insert_provider_witness("mock", &witness).unwrap();
    [
        ("Bucket", SchemaKind::Resource),
        ("Lookup", SchemaKind::DataSource),
    ]
    .into_iter()
    .map(|(resource, kind)| {
        registry
            .get("mock", resource, kind)
            .unwrap()
            .semantic_fingerprint()
    })
    .collect()
}

fn fingerprint_edge(source: &str, sink: &str) -> carina_core::schema::AssignabilityEdge {
    carina_core::schema::AssignabilityEdge::new(identity(source), identity(sink)).unwrap()
}

#[test]
fn owner_assignability_edges_change_schema_fingerprints() {
    let without = schema_fingerprints(vec![]);
    let forward = schema_fingerprints(vec![fingerprint_edge("NewName", "Name")]);
    let reverse = schema_fingerprints(vec![fingerprint_edge("Name", "NewName")]);
    for index in 0..without.len() {
        assert_ne!(
            without[index], forward[index],
            "adding an owner edge must change the fingerprint"
        );
        assert_ne!(
            forward[index], reverse[index],
            "changing an owner edge must change the fingerprint"
        );
    }
}

#[test]
fn owner_assignability_edge_order_does_not_change_schema_fingerprints() {
    assert_eq!(
        schema_fingerprints(vec![
            fingerprint_edge("NewName", "Name"),
            fingerprint_edge("A", "B")
        ]),
        schema_fingerprints(vec![
            fingerprint_edge("A", "B"),
            fingerprint_edge("NewName", "Name")
        ]),
    );
}

#[test]
fn owner_declared_edges_change_fingerprints_even_when_reachability_is_equal() {
    let transitive =
        schema_fingerprints(vec![fingerprint_edge("A", "B"), fingerprint_edge("B", "C")]);
    let explicit = schema_fingerprints(vec![
        fingerprint_edge("A", "B"),
        fingerprint_edge("B", "C"),
        fingerprint_edge("A", "C"),
    ]);
    for index in 0..transitive.len() {
        assert_ne!(
            transitive[index], explicit[index],
            "fingerprints must include declared edges"
        );
    }
}

static P_OWNER: std::sync::LazyLock<SchemaTypeOwner> =
    std::sync::LazyLock::new(|| SchemaTypeOwner::Provider("p".into()));
static Q_OWNER: std::sync::LazyLock<SchemaTypeOwner> =
    std::sync::LazyLock::new(|| SchemaTypeOwner::Provider("q".into()));

// Construct sinks through a registered resource owner, as production callers do.
fn assignment(
    source: &AttributeType,
    sink: &AttributeType,
    catalog: &ProviderCustomTypes,
    owner: &SchemaTypeOwner,
) -> Result<bool, Box<carina_core::parser::CustomTypeValidationError>> {
    let SchemaTypeOwner::Provider(owner) = owner;
    let mut registry = carina_core::schema::SchemaRegistry::new();
    registry.insert(
        owner,
        carina_core::schema::ResourceSchema::new("Assignment")
            .attribute(
                carina_core::schema::AttributeSchema::optional("source", source.clone()).build(),
            )
            .attribute(carina_core::schema::AttributeSchema::optional("sink", sink.clone()).build())
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get(owner, "Assignment", SchemaKind::Resource)
        .unwrap();
    schema
        .as_schema()
        .type_in_schema(schema.as_schema().attributes()["source"].attr_type())
        .assignment_to(
            schema.assignment_sink(schema.as_schema().attributes()["sink"].attr_type(), catalog),
        )
        .require_loaded()
        .map_err(Box::new)
}

#[test]
fn builtin_annotation_leaves_compare_without_provider_owner_evidence() {
    use carina_core::schema::CompiledAttributeType;
    let catalog = ProviderCustomTypes::default();
    let sink = carina_core::validation::lift_type_expr(&TypeExpr::Simple(
        carina_core::parser::BuiltinTypeName::lookup("positive_int").unwrap(),
    ))
    .unwrap();
    let custom = CompiledAttributeType::refined_int(Some(identity("Count")), Some((Some(1), None)));
    assert_eq!(
        TypeInSchema::schemaless(&custom)
            .assignment_to(sink.assignment_sink(&catalog))
            .require_loaded(),
        Ok(false)
    );
    assert_eq!(
        TypeInSchema::schemaless(&sink)
            .assignment_to(sink.assignment_sink(&catalog))
            .require_loaded(),
        Ok(true)
    );
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert(
        "p",
        carina_core::schema::ResourceSchema::new("Count")
            .validate()
            .unwrap(),
    );
    let schema = schemas.get("p", "Count", SchemaKind::Resource).unwrap();
    let provider_sink = CompiledAttributeType::refined_int(
        Some(carina_core::schema::TypeIdentity::bare("PositiveInt")),
        None,
    );
    assert!(matches!(
        TypeInSchema::schemaless(&custom)
            .assignment_to(schema.assignment_sink(&provider_sink, &catalog))
            .require_loaded(),
        Err(carina_core::parser::CustomTypeValidationError::ProvidersNotLoaded),
    ));
}

#[test]
fn resource_and_data_source_sinks_retain_registered_typed_owners() {
    let loaded = snapshot(&[("p", true), ("q", false)]);
    for (provider, accepted) in [("p", true), ("q", false)] {
        for (resource_type, kind) in [
            ("Bucket", SchemaKind::Resource),
            ("Lookup", SchemaKind::DataSource),
        ] {
            let schema = loaded
                .schema_registry()
                .get(provider, resource_type, kind)
                .unwrap();
            let sink = schema.as_schema().attributes()["bucket"].attr_type();
            let source = schema.as_schema().attributes()["new_bucket"].attr_type();
            assert_eq!(
                schema.schema_type_owner(),
                &SchemaTypeOwner::Provider(provider.into())
            );
            assert_eq!(
                TypeInSchema::schemaless(source)
                    .assignment_to(
                        schema.assignment_sink(sink, &loaded.provider_context().custom_types)
                    )
                    .require_loaded(),
                Ok(accepted)
            );
        }
    }
}

#[test]
fn scalar_identity_edges_never_relax_enum_comparisons() {
    let loaded = snapshot(&[("p", true)]);
    let bootstrap = ProviderCustomTypes::default();
    let enum_type =
        |kind| AttributeType::enum_(identity(kind), Some(vec!["on".into()]), vec![], None, None);
    for catalog in [&loaded.provider_context().custom_types, &bootstrap] {
        for (source, sink) in [
            (enum_type("NewName"), enum_type("Name")),
            (string("NewName"), enum_type("Name")),
            (enum_type("NewName"), string("Name")),
        ] {
            assert_eq!(assignment(&source, &sink, catalog, &P_OWNER), Ok(false));
        }
    }
}

#[test]
fn identity_edges_are_directional_and_do_not_admit_unrelated_types() {
    let loaded = snapshot(&[("p", true)]);
    let catalog = &loaded.provider_context().custom_types;
    assert_eq!(
        assignment(&string("NewName"), &string("Name"), catalog, &P_OWNER),
        Ok(true)
    );
    assert_eq!(
        assignment(&string("Name"), &string("NewName"), catalog, &P_OWNER),
        Ok(false)
    );
    assert_eq!(
        assignment(&string("Other"), &string("Name"), catalog, &P_OWNER),
        Ok(false)
    );
}

#[test]
fn identity_edges_compose_transitively_within_the_sink_owner() {
    let loaded = snapshot(&[("p", true)]);
    assert_eq!(
        assignment(
            &string("A"),
            &string("C"),
            &loaded.provider_context().custom_types,
            &P_OWNER
        ),
        Ok(true)
    );
}

#[test]
fn identity_edges_use_the_attribute_owner_instead_of_a_co_owner() {
    let loaded = snapshot(&[("p", true), ("q", false)]);
    let catalog = &loaded.provider_context().custom_types;
    assert_eq!(
        assignment(&string("NewName"), &string("Name"), catalog, &Q_OWNER),
        Ok(false)
    );
    assert_eq!(
        assignment(&string("NewName"), &string("Name"), catalog, &P_OWNER),
        Ok(true)
    );
}

#[test]
fn identity_edges_require_every_annotation_owner() {
    for q_edges in [false, true] {
        let loaded = snapshot(&[("p", true), ("q", q_edges)]);
        let catalog = &loaded.provider_context().custom_types;
        let owned = catalog
            .known()
            .iter()
            .find(|owned| owned.identity() == &identity("Name"))
            .unwrap();
        assert_eq!(owned.owners().iter().len(), 2);
        let sink = carina_core::validation::lift_type_expr(&TypeExpr::SchemaType {
            owned: owned.clone(),
        })
        .unwrap();
        let schema = loaded
            .schema_registry()
            .get("p", "Bucket", SchemaKind::Resource)
            .unwrap();
        let source = schema.as_schema().attributes()["new_name"].attr_type();
        assert_eq!(
            TypeInSchema::schemaless(source)
                .assignment_to(sink.assignment_sink(catalog))
                .require_loaded(),
            Ok(q_edges)
        );
    }
}

#[test]
fn identity_edges_preserve_rule_seven_length_containment() {
    let loaded = snapshot(&[("p", true)]);
    let source = AttributeType::refined_string(
        Some(identity("NewName")),
        None,
        Some((Some(1), Some(20))),
        None,
    );
    let sink = AttributeType::refined_string(
        Some(identity("Name")),
        None,
        Some((Some(1), Some(10))),
        None,
    );
    assert_eq!(
        assignment(
            &source,
            &sink,
            &loaded.provider_context().custom_types,
            &P_OWNER
        ),
        Ok(false)
    );
}

#[test]
fn identity_edges_apply_to_int_and_float() {
    let loaded = snapshot(&[("p", true)]);
    let schema = loaded
        .schema_registry()
        .get("p", "Bucket", SchemaKind::Resource)
        .unwrap();
    for (source, sink) in [("new_count", "count"), ("new_ratio", "ratio")] {
        let source = schema.as_schema().attributes()[source].attr_type();
        let sink = schema.as_schema().attributes()[sink].attr_type();
        assert!(
            TypeInSchema::schemaless(source).is_assignable_to(
                schema.assignment_sink(sink, &loaded.provider_context().custom_types)
            ),
            "{source:?} -> {sink:?}"
        );
        assert!(!TypeInSchema::schemaless(sink).is_assignable_to(
            schema.assignment_sink(source, &loaded.provider_context().custom_types)
        ));
    }
}

#[test]
fn identity_edges_defer_when_the_sink_owner_is_unavailable() {
    let source = string("NewName");
    let sink = string("Name");
    assert!(matches!(
        assignment(&source, &sink, &ProviderCustomTypes::default(), &P_OWNER),
        Err(error) if *error == carina_core::parser::CustomTypeValidationError::ProvidersNotLoaded
    ));
    let loaded = snapshot(&[("p", true), ("q", false)]);
    let partial = loaded
        .provider_context()
        .custom_types
        .with_failed_providers(std::collections::BTreeSet::from(["p".into()]));
    assert!(matches!(
        assignment(&source, &sink, &partial, &P_OWNER),
        Err(error) if *error == carina_core::parser::CustomTypeValidationError::ProvidersNotLoaded
    ));
    assert_eq!(assignment(&source, &sink, &partial, &Q_OWNER), Ok(false));
}

#[test]
fn identity_edges_recurse_through_containers_and_refs() {
    let loaded = snapshot(&[("p", true)]);
    let source = AttributeType::list(AttributeType::struct_(
        "Source",
        vec![StructField::required("value", AttributeType::ref_("Leaf")).build()],
    ));
    let sink = AttributeType::list(AttributeType::struct_(
        "Sink",
        vec![StructField::required("value", AttributeType::ref_("Leaf")).build()],
    ));
    let mut registry = carina_core::schema::SchemaRegistry::new();
    let source_schema = carina_core::schema::ResourceSchema::new("Source")
        .with_def("Leaf", string("NewName"))
        .attribute(carina_core::schema::AttributeSchema::optional("value", source).build())
        .validate()
        .unwrap();
    registry.insert(
        "p",
        carina_core::schema::ResourceSchema::new("Sink")
            .with_def("Leaf", string("Name"))
            .attribute(carina_core::schema::AttributeSchema::optional("value", sink).build())
            .validate()
            .unwrap(),
    );
    let sink_schema = registry.get("p", "Sink", SchemaKind::Resource).unwrap();
    assert!(
        source_schema
            .as_schema()
            .type_in_schema(source_schema.as_schema().attributes()["value"].attr_type())
            .is_assignable_to(sink_schema.assignment_sink(
                sink_schema.as_schema().attributes()["value"].attr_type(),
                &loaded.provider_context().custom_types,
            ))
    );
    assert!(loaded.provider_context().custom_types.is_loaded());
}

#[test]
fn identity_edges_loaded_catalog_without_a_closure_rejects_mismatches() {
    let loaded = snapshot(&[("q", false)]);
    let source = string("NewName");
    let sink = string("Name");
    assert_eq!(
        assignment(
            &source,
            &sink,
            &loaded.provider_context().custom_types,
            &P_OWNER
        ),
        Ok(false)
    );
}

#[test]
fn identity_edges_retain_each_annotation_leafs_owner_set() {
    use carina_core::parser::{NonEmptySchemaTypeOwners, SchemaTypeOwner};
    let loaded = snapshot(&[("p", true), ("q", false)]);
    let catalog = &loaded.provider_context().custom_types;
    let both = catalog
        .known()
        .iter()
        .find(|owned| owned.identity() == &identity("Name"))
        .unwrap()
        .clone();
    let mut wire = serde_json::to_value(&both).unwrap();
    wire["owners"] = serde_json::to_value(NonEmptySchemaTypeOwners::singleton(
        SchemaTypeOwner::Provider("p".into()),
    ))
    .unwrap();
    let p_only: carina_core::parser::OwnedTypeIdentity = serde_json::from_value(wire).unwrap();
    let source = AttributeType::struct_(
        "Source",
        vec![
            StructField::required("first", string("NewName")).build(),
            StructField::required(
                "second",
                AttributeType::list(AttributeType::map(string("NewName"))),
            )
            .build(),
        ],
    );
    for (second, accepted) in [(both, false), (p_only.clone(), true)] {
        let sink = TypeExpr::Struct {
            fields: vec![
                (
                    "first".into(),
                    TypeExpr::SchemaType {
                        owned: p_only.clone(),
                    },
                ),
                (
                    "second".into(),
                    TypeExpr::List(Box::new(TypeExpr::Map(Box::new(TypeExpr::SchemaType {
                        owned: second,
                    })))),
                ),
            ],
        };
        let sink = carina_core::validation::lift_type_expr(&sink).unwrap();
        let source_schema = carina_core::schema::ResourceSchema::new("Source")
            .attribute(
                carina_core::schema::AttributeSchema::optional("value", source.clone()).build(),
            )
            .validate()
            .unwrap();
        assert_eq!(
            source_schema
                .as_schema()
                .type_in_schema(source_schema.as_schema().attributes()["value"].attr_type())
                .is_assignable_to(sink.assignment_sink(catalog)),
            accepted
        );
    }
}

#[test]
fn identity_edges_preserve_union_all_and_any_rules() {
    let loaded = snapshot(&[("p", true)]);
    let catalog = &loaded.provider_context().custom_types;
    let source = AttributeType::union(vec![string("NewName"), string("Name")]);
    assert_eq!(
        assignment(&source, &string("Name"), catalog, &P_OWNER),
        Ok(true)
    );
    let source = AttributeType::union(vec![string("NewName"), string("Other")]);
    assert_eq!(
        assignment(&source, &string("Name"), catalog, &P_OWNER),
        Ok(false)
    );
    let sink = AttributeType::union(vec![string("Other"), string("Name")]);
    assert_eq!(
        assignment(&string("NewName"), &sink, catalog, &P_OWNER),
        Ok(true)
    );
}
