use carina_smithy::*;

fn load_minimal() -> SmithyModel {
    let json = include_str!("fixtures/minimal.json");
    parse(json).expect("Failed to parse minimal fixture")
}

#[test]
fn test_parse_version() {
    let model = load_minimal();
    assert_eq!(model.smithy, "2.0");
}

#[test]
fn test_shape_count() {
    let model = load_minimal();
    // service(1) + operations(3) + structures(7) + string(1) + enums(2) + list(1) + map(1) + union(1)
    assert_eq!(model.shapes.len(), 17);
}

#[test]
fn test_find_service() {
    let model = load_minimal();
    let (id, service) = model.find_service().expect("No service found");
    assert_eq!(id, "com.example#MyService");
    assert_eq!(service.version, "2024-01-01");
    assert_eq!(service.operations.len(), 3);
}

#[test]
fn test_service_operations() {
    let model = load_minimal();
    let ops = model
        .service_operations("com.example#MyService")
        .expect("No operations");
    assert_eq!(ops.len(), 3);
    assert!(ops.contains(&"com.example#CreateThing"));
    assert!(ops.contains(&"com.example#DescribeThing"));
    assert!(ops.contains(&"com.example#DeleteThing"));
}

#[test]
fn test_get_operation() {
    let model = load_minimal();
    let op = model
        .get_operation("com.example#CreateThing")
        .expect("Operation not found");
    assert_eq!(
        op.input.as_ref().unwrap().target,
        "com.example#CreateThingRequest"
    );
    assert_eq!(
        op.output.as_ref().unwrap().target,
        "com.example#CreateThingResult"
    );
}

#[test]
fn test_operation_input() {
    let model = load_minimal();
    let input = model
        .operation_input("com.example#CreateThing")
        .expect("No input");
    assert_eq!(input.members.len(), 4);
    assert!(input.members.contains_key("Name"));
    assert!(input.members.contains_key("Color"));
    assert!(input.members.contains_key("Tags"));
    assert!(input.members.contains_key("Metadata"));
    assert!(SmithyModel::is_input(input));
}

#[test]
fn test_operation_output() {
    let model = load_minimal();
    let output = model
        .operation_output("com.example#CreateThing")
        .expect("No output");
    assert_eq!(output.members.len(), 1);
    assert!(output.members.contains_key("Thing"));
    assert!(SmithyModel::is_output(output));
}

#[test]
fn test_operation_unit_output() {
    let model = load_minimal();
    // DeleteThing has smithy.api#Unit output = no output
    assert!(model.operation_output("com.example#DeleteThing").is_none());
    assert!(
        model
            .operation_output_id("com.example#DeleteThing")
            .is_none()
    );
}

#[test]
fn test_required_members() {
    let model = load_minimal();
    let input = model
        .operation_input("com.example#CreateThing")
        .expect("No input");

    let name = &input.members["Name"];
    assert!(SmithyModel::is_required(name));

    let color = &input.members["Color"];
    assert!(!SmithyModel::is_required(color));
}

#[test]
fn test_documentation() {
    let model = load_minimal();
    let op = model
        .get_operation("com.example#CreateThing")
        .expect("Op not found");
    assert_eq!(
        SmithyModel::documentation(&op.traits),
        Some("Creates a thing")
    );
}

#[test]
fn test_enum_values() {
    let model = load_minimal();
    let values = model
        .enum_values("com.example#Color")
        .expect("Enum not found");
    assert_eq!(values.len(), 3);

    let value_strings: Vec<&str> = values.iter().map(|(_, v)| v.as_str()).collect();
    assert!(value_strings.contains(&"red"));
    assert!(value_strings.contains(&"green"));
    assert!(value_strings.contains(&"blue"));
}

#[test]
fn test_structure_members() {
    let model = load_minimal();
    let thing = model
        .get_structure("com.example#Thing")
        .expect("Thing not found");
    assert_eq!(thing.members.len(), 5);
    assert_eq!(thing.members["ThingId"].target, "com.example#ThingId");
    assert_eq!(thing.members["Name"].target, "smithy.api#String");
    assert_eq!(thing.members["Color"].target, "com.example#Color");
    assert_eq!(thing.members["Status"].target, "com.example#ThingStatus");
    assert_eq!(thing.members["CreatedAt"].target, "smithy.api#Timestamp");
}

#[test]
fn test_shape_kind() {
    let model = load_minimal();

    assert_eq!(
        model.shape_kind("com.example#MyService"),
        Some(ShapeKind::Service)
    );
    assert_eq!(
        model.shape_kind("com.example#CreateThing"),
        Some(ShapeKind::Operation)
    );
    assert_eq!(
        model.shape_kind("com.example#Thing"),
        Some(ShapeKind::Structure)
    );
    assert_eq!(model.shape_kind("com.example#Color"), Some(ShapeKind::Enum));
    assert_eq!(
        model.shape_kind("com.example#TagList"),
        Some(ShapeKind::List)
    );
    assert_eq!(
        model.shape_kind("com.example#MetadataMap"),
        Some(ShapeKind::Map)
    );
    assert_eq!(
        model.shape_kind("com.example#ThingId"),
        Some(ShapeKind::String)
    );
    assert_eq!(
        model.shape_kind("com.example#Filter"),
        Some(ShapeKind::Union)
    );

    // Prelude types
    assert_eq!(
        model.shape_kind("smithy.api#String"),
        Some(ShapeKind::String)
    );
    assert_eq!(
        model.shape_kind("smithy.api#Boolean"),
        Some(ShapeKind::Boolean)
    );
    assert_eq!(
        model.shape_kind("smithy.api#Integer"),
        Some(ShapeKind::Integer)
    );
    assert_eq!(
        model.shape_kind("smithy.api#Timestamp"),
        Some(ShapeKind::Timestamp)
    );
    assert_eq!(model.shape_kind("smithy.api#Unit"), Some(ShapeKind::Unit));

    // Non-existent
    assert_eq!(model.shape_kind("com.example#DoesNotExist"), None);
}

#[test]
fn test_shape_name() {
    assert_eq!(SmithyModel::shape_name("com.amazonaws.ec2#Vpc"), "Vpc");
    assert_eq!(
        SmithyModel::shape_name("com.amazonaws.ec2#CreateVpcRequest"),
        "CreateVpcRequest"
    );
    assert_eq!(SmithyModel::shape_name("smithy.api#String"), "String");
    assert_eq!(SmithyModel::shape_name("NoHash"), "NoHash");
}

#[test]
fn test_shape_namespace() {
    assert_eq!(
        SmithyModel::shape_namespace("com.amazonaws.ec2#Vpc"),
        "com.amazonaws.ec2"
    );
    assert_eq!(
        SmithyModel::shape_namespace("smithy.api#String"),
        "smithy.api"
    );
    assert_eq!(SmithyModel::shape_namespace("NoHash"), "NoHash");
}

#[test]
fn test_list_shape() {
    let model = load_minimal();
    if let Some(Shape::List(list)) = model.get_shape("com.example#TagList") {
        assert_eq!(list.member.target, "com.example#Tag");
    } else {
        panic!("TagList should be a list shape");
    }
}

#[test]
fn test_map_shape() {
    let model = load_minimal();
    if let Some(Shape::Map(map)) = model.get_shape("com.example#MetadataMap") {
        assert_eq!(map.key.target, "smithy.api#String");
        assert_eq!(map.value.target, "smithy.api#String");
    } else {
        panic!("MetadataMap should be a map shape");
    }
}

#[test]
fn test_union_shape() {
    let model = load_minimal();
    if let Some(Shape::Union(union_shape)) = model.get_shape("com.example#Filter") {
        assert_eq!(union_shape.members.len(), 2);
        assert!(union_shape.members.contains_key("ByName"));
        assert!(union_shape.members.contains_key("ByColor"));
    } else {
        panic!("Filter should be a union shape");
    }
}
