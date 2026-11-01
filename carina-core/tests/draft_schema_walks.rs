use std::collections::HashSet;

use carina_core::resource::{ConcreteValue, EnumValueResolver, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, LeafPath, LeafPathSegment, ResourceSchema, Shape,
    ShapeWalkBudget, StructField, TypeInSchema, enum_identity, schema_write_only_keys, types,
};
use indexmap::IndexMap;

fn string_with_invalid_pattern() -> AttributeType {
    AttributeType::refined_string(None, Some("(?=x)".into()), None, None)
}

#[test]
fn builtin_type_helpers_infer_draft_without_context() {
    macro_rules! assert_string_type {
        ($constructor:ident, $name:literal) => {
            let t = types::$constructor();
            assert!(matches!(
                t.shape_ref_free().unwrap(),
                Shape::String { identity: Some(identity), .. } if identity.kind == $name
            ));
        };
    }

    let t = types::positive_int();
    assert!(matches!(
        t.shape_ref_free().unwrap(),
        Shape::Int { identity: Some(identity), .. } if identity.kind == "PositiveInt"
    ));
    assert_string_type!(ipv4_cidr, "Ipv4Cidr");
    assert_string_type!(ipv4_address, "Ipv4Address");
    assert_string_type!(ipv6_address, "Ipv6Address");
    assert_string_type!(ipv6_cidr, "Ipv6Cidr");
    assert_string_type!(email, "Email");
    assert_string_type!(http_response_status_code, "HttpResponseStatusCode");
    assert_string_type!(redirect_protocol, "RedirectProtocol");
    assert_string_type!(redirect_host, "RedirectHost");
    assert_string_type!(redirect_port, "RedirectPort");
    assert_string_type!(redirect_path, "RedirectPath");
    assert_string_type!(redirect_query, "RedirectQuery");

    let t = types::cidr();
    assert!(matches!(t.shape_ref_free().unwrap(), Shape::Union));
}

#[test]
fn draft_ref_free_walks_preserve_patterns_and_budget() {
    let string = string_with_invalid_pattern();
    let shape: Shape<'_> = string.shape_ref_free().unwrap();
    assert!(matches!(
        shape,
        Shape::String {
            pattern: Some("(?=x)"),
            ..
        }
    ));
    let context: TypeInSchema<'_> = TypeInSchema::schemaless(&string);
    assert!(context.is_assignable_to(TypeInSchema::schemaless(&string)));

    let structure =
        AttributeType::struct_("Draft", vec![StructField::optional("name", string).build()]);
    let mut budget = ShapeWalkBudget::new(1);
    let fields = structure
        .struct_fields_ref_free_with_budget(&mut budget)
        .unwrap()
        .unwrap();
    assert_eq!(fields[0].name(), "name");
    assert!(
        structure
            .struct_fields_ref_free_with_budget(&mut budget)
            .unwrap()
            .is_none()
    );

    let union = AttributeType::union(vec![AttributeType::ref_("Missing"), structure]);
    let mut budget = ShapeWalkBudget::new(1);
    let members = union
        .union_members_ref_free_with_budget(&mut budget)
        .unwrap()
        .unwrap();
    let resolved = members.iter().collect::<Vec<_>>();
    assert!(resolved[0].is_none());
    assert!(matches!(
        resolved[1].unwrap().as_attr().shape_ref_free().unwrap(),
        Shape::Struct { name: "Draft" }
    ));
    assert!(
        union
            .union_members_ref_free_with_budget(&mut budget)
            .unwrap()
            .is_none()
    );
    assert!(AttributeType::ref_("Missing").shape_ref_free().is_err());
}

fn struct_field_type<'a>(
    schema: &'a ResourceSchema,
    attribute: &'a AttributeType,
    name: &str,
) -> Option<&'a AttributeType> {
    schema
        .struct_fields_with_budget(attribute, &mut ShapeWalkBudget::new(8))?
        .iter()
        .find(|field| field.name() == name)
        .map(StructField::field_type)
}

#[test]
fn draft_schema_walks_resolve_definitions() {
    let schema = ResourceSchema::new("test.Draft")
        .with_def("Alias", AttributeType::ref_("Credentials"))
        .with_def(
            "Credentials",
            AttributeType::struct_(
                "Credentials",
                vec![
                    StructField::optional("secret", string_with_invalid_pattern()).build(),
                    StructField::optional(
                        "children",
                        AttributeType::list(AttributeType::ref_("Credentials")),
                    )
                    .build(),
                ],
            ),
        )
        .with_def(
            "Choice",
            AttributeType::union(vec![AttributeType::ref_("Alias"), AttributeType::bool()]),
        );
    let reference = AttributeType::ref_("Alias");
    let resolved: &AttributeType = schema.resolve_of(&reference).as_attr();
    let shape: Shape<'_> = schema.shape_of(resolved);
    assert!(matches!(
        shape,
        Shape::Struct {
            name: "Credentials"
        }
    ));
    assert_eq!(
        schema.type_in_schema(&reference).resolved_type_name(),
        "Struct(Credentials)"
    );
    let secret = struct_field_type(&schema, &reference, "secret").unwrap();
    assert!(
        schema
            .type_in_schema(secret)
            .is_assignable_to(secret.in_schema(schema.defs()))
    );
    assert!(matches!(
        secret.shape_ref_free().unwrap(),
        Shape::String {
            pattern: Some("(?=x)"),
            ..
        }
    ));
    let children = struct_field_type(&schema, &reference, "children").unwrap();
    let Shape::List { element_type, .. } = schema.shape_of(children) else {
        panic!("children must remain a list");
    };
    assert!(matches!(
        schema.shape_of(element_type),
        Shape::Struct { .. }
    ));

    let choice = AttributeType::ref_("Choice");
    let mut budget = ShapeWalkBudget::new(1);
    let members = schema
        .union_members_with_budget(&choice, &mut budget)
        .unwrap();
    let shapes = members
        .iter()
        .map(|member| schema.shape_of(member.unwrap().as_attr()))
        .collect::<Vec<_>>();
    assert!(matches!(
        shapes.as_slice(),
        [Shape::Struct { .. }, Shape::Bool]
    ));
    assert!(
        schema
            .union_members_with_budget(&choice, &mut budget)
            .is_none()
    );
}

#[test]
fn draft_metadata_walks_support_write_only_and_enum_aliases() {
    let identity = enum_identity("Mode", Some("test"));
    let mode = AttributeType::enum_(
        identity.clone(),
        Some(vec!["ACTIVE".into()]),
        vec![("ACTIVE".into(), "active".into())],
        None,
        None,
    );
    let resolver: EnumValueResolver<'_> = EnumValueResolver::new(&mode);
    assert_eq!(resolver.enum_identity(), Some(identity.clone()));
    let schema = ResourceSchema::new("test.Draft")
        .with_def("Mode", mode)
        .with_def(
            "Config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional("secret", string_with_invalid_pattern())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .attribute(AttributeSchema::optional("config", AttributeType::ref_("Config")).build())
        .attribute(
            AttributeSchema::optional("modes", AttributeType::list(AttributeType::ref_("Mode")))
                .build(),
        )
        .attribute(
            AttributeSchema::optional("password", string_with_invalid_pattern())
                .write_only()
                .build(),
        );
    assert_eq!(
        schema_write_only_keys(Some(&schema)),
        HashSet::from(["password".into()])
    );
    assert_eq!(
        schema.enum_valid_values_for_attr_alias("modes", "test.Mode.active"),
        ["ACTIVE"]
    );
    let mode_ref = AttributeType::ref_("Mode");
    assert_eq!(
        EnumValueResolver::with_defs(&mode_ref, schema.defs()).enum_identity(),
        Some(identity)
    );

    let secret = Value::Concrete(ConcreteValue::String("unmatched".into()));
    let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        "secret".into(),
        secret.clone(),
    )])));
    let leaves: Vec<(LeafPath, Value, &AttributeType)> =
        schema.write_only_leaf_projection("config", &value);
    assert_eq!(leaves.len(), 1);
    assert_eq!(
        leaves[0].0.segments(),
        &[LeafPathSegment::StructField("secret".into())]
    );
    assert_eq!(leaves[0].1, secret);
    assert!(matches!(
        leaves[0].2.shape_ref_free().unwrap(),
        Shape::String {
            pattern: Some("(?=x)"),
            ..
        }
    ));
    assert_eq!(schema.validate().unwrap_err().pattern_errors().count(), 2);
}
