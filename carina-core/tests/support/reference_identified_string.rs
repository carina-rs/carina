//! Shared directory fixtures for Reference -> identified String admission (#4384).
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
};

pub const UNKNOWN_SOURCE: &str = "let r = mock.test.reference { name = 'r' }\n";
pub const KNOWN_SOURCE: &str = "let r = mock.test.reference { name = 'r' key = 'k' }\n";

pub fn key_identity() -> TypeIdentity {
    TypeIdentity::from_dotted("mock.keys.Key.Reference")
}

pub fn identified_string(identity: TypeIdentity) -> AttributeType {
    AttributeType::refined_string(Some(identity), None, None, None)
}

pub fn shapes() -> Vec<(&'static str, AttributeType, &'static str)> {
    shapes_for(identified_string(TypeIdentity::from_dotted(
        "mock.RecoveryProbe",
    )))
}

pub fn shapes_for(target: AttributeType) -> Vec<(&'static str, AttributeType, &'static str)> {
    let record = || {
        AttributeType::struct_(
            "Record",
            vec![StructField::required("field", target.clone()).build()],
        )
    };
    vec![
        ("scalar", target.clone(), "r.key"),
        ("list", AttributeType::list(target.clone()), "[r.key]"),
        (
            "map",
            AttributeType::map(target.clone()),
            "{ field = r.key }",
        ),
        ("struct", record(), "{ field = r.key }"),
        ("secret", target.clone(), "secret(r.key)"),
        (
            "secret_list",
            AttributeType::list(target.clone()),
            "secret([r.key])",
        ),
        ("ref", AttributeType::ref_("Target"), "r.key"),
        (
            "recursive",
            AttributeType::ref_("Node"),
            "{ field = r.key children = [] }",
        ),
        (
            "recursive_union",
            AttributeType::ref_("RecursiveValue"),
            "[r.key]",
        ),
        (
            "union_list",
            AttributeType::union(vec![
                AttributeType::bool(),
                AttributeType::list(target.clone()),
            ]),
            "[r.key]",
        ),
        (
            "union_map",
            AttributeType::union(vec![
                AttributeType::bool(),
                AttributeType::map(target.clone()),
            ]),
            "{ field = r.key }",
        ),
        (
            "union_struct",
            AttributeType::union(vec![AttributeType::bool(), record()]),
            "{ field = r.key }",
        ),
    ]
}

pub fn schemas(value_type: AttributeType) -> SchemaRegistry {
    let target = identified_string(TypeIdentity::from_dotted("mock.RecoveryProbe"));
    schemas_with_target(value_type, target)
}

pub fn schemas_with_target(value_type: AttributeType, target: AttributeType) -> SchemaRegistry {
    let reference = AttributeType::reference(key_identity(), None, None);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.reference")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("key", reference).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.recovery_probe")
            .attribute(AttributeSchema::optional("value", value_type).build())
            .with_def("Target", target.clone())
            .with_def(
                "RecursiveValue",
                AttributeType::union(vec![
                    target.clone(),
                    AttributeType::list(AttributeType::ref_("RecursiveValue")),
                ]),
            )
            .with_def(
                "Node",
                AttributeType::struct_(
                    "Node",
                    vec![
                        StructField::optional("field", target).build(),
                        StructField::optional(
                            "children",
                            AttributeType::list(AttributeType::ref_("Node")),
                        )
                        .build(),
                    ],
                ),
            )
            .validate()
            .unwrap(),
    );
    for (name, ty) in [
        ("PlainStringSink", AttributeType::string()),
        ("SameStringSink", identified_string(key_identity())),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(AttributeSchema::optional("value", ty).build())
                .validate()
                .unwrap(),
        );
    }
    schemas
}

pub fn accepted_targets() -> Vec<(&'static str, AttributeType, &'static str)> {
    vec![
        ("identityless", AttributeType::string(), "r.key"),
        ("same identity", identified_string(key_identity()), "r.key"),
        (
            "Reference to Reference",
            AttributeType::reference(TypeIdentity::from_dotted("mock.OtherReference"), None, None),
            "r.key",
        ),
        (
            "String to Reference",
            AttributeType::reference(TypeIdentity::from_dotted("mock.OtherReference"), None, None),
            "r.name",
        ),
    ]
}

pub fn differently_identified_reference_input_sources() -> Vec<(&'static str, AttributeType)> {
    vec![
        (
            "String",
            identified_string(TypeIdentity::from_dotted("mock.StringSource")),
        ),
        (
            "Reference",
            AttributeType::reference(key_identity(), None, None),
        ),
    ]
}

pub fn differently_identified_reference_input_schemas(
    source_type: AttributeType,
) -> SchemaRegistry {
    let mut schemas = schemas(AttributeType::reference(
        TypeIdentity::from_dotted("mock.OtherReference"),
        None,
        None,
    ));
    schemas.insert(
        "mock",
        ResourceSchema::new("InputSource")
            .attribute(AttributeSchema::optional("value", source_type).build())
            .validate()
            .unwrap(),
    );
    schemas
}

pub fn directory(main: &str, source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (file, text) in [
        ("main.crn", main),
        ("sources.crn", source),
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ] {
        std::fs::write(dir.path().join(file), text).unwrap();
    }
    dir
}

pub fn consumer(value: &str) -> String {
    format!("mock.test.recovery_probe {{\n  value = {value}\n}}\n")
}

#[allow(dead_code)] // Consumed by the CLI harness; this fixture is shared with LSP tests.
pub fn cli_messages(shape: &str, known: bool) -> Vec<String> {
    let owner = "mock.test.recovery_probe.<pending>: ";
    let sink = if shape == "recursive_union" {
        "mock.RecoveryProbe | List<mock.RecoveryProbe | List<Ref(RecursiveValue)>>"
    } else {
        "mock.RecoveryProbe"
    };
    let mismatch = format!(
        "cannot assign mock.keys.Key.Reference to 'value': expected {sink}, got mock.keys.Key.Reference"
    );
    let origin = match shape {
        "secret" => " (from (secret))",
        "secret_list" if !known => "",
        "secret_list" => " (from (secret))",
        _ => " (from r.key)",
    };
    let mut messages = Vec::new();
    // origin/main (e0dd9dd71) already adds this unknown-only CLI union finding
    // for rejected plain String sources (carina#4397); known CLI values and LSP keep one finding.
    if !known {
        let container = match shape {
            "union_list" | "recursive_union" => Some("List item at index 0"),
            "union_map" => Some("Map value for key 'field'"),
            "union_struct" => Some("Struct field 'field'"),
            _ => None,
        };
        if let Some(container) = container {
            messages.push(format!(
                "{owner}{container}: Type mismatch: expected {sink}, got mock.keys.Key.Reference"
            ));
        }
    }
    messages.push(format!("{owner}{mismatch}{origin}"));
    messages
}

pub fn lsp_message(shape: &str) -> &'static str {
    match shape {
        "recursive_union" => {
            "cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe | List<mock.RecoveryProbe | List<Ref(RecursiveValue)>>, got mock.keys.Key.Reference (from r.key)"
        }
        "scalar" | "ref" => {
            "Type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)"
        }
        "secret" => "Type mismatch: expected mock.RecoveryProbe, got mock.keys.Key.Reference",
        "secret_list" => {
            "cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference"
        }
        _ => {
            "cannot assign mock.keys.Key.Reference to 'value': expected mock.RecoveryProbe, got mock.keys.Key.Reference (from r.key)"
        }
    }
}
