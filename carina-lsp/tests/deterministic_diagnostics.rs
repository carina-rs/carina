mod support;

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use support::fixture::{analyze, engine_with_schemas, write_fixture};

#[test]
fn undefined_function_references_keep_sibling_declaration_order() {
    let fixture = write_fixture(&[]);
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order/functions/undefined_references");
    for file in std::fs::read_dir(source).unwrap() {
        let file = file.unwrap();
        std::fs::copy(file.path(), fixture.path().join(file.file_name())).unwrap();
    }
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = engine_with_schemas(schemas);
    for (file, names) in [
        (
            "a_functions.crn",
            &[
                "missing_zebra",
                "missing_mango",
                "missing_quartz",
                "missing_delta",
            ][..],
        ),
        ("z_functions.crn", &["missing_alpha", "missing_bravo"][..]),
    ] {
        let diagnostics = analyze(&engine, &fixture, file);
        let undefined = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.starts_with("Undefined resource:"))
            .collect::<Vec<_>>();
        assert_eq!(
            undefined
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            names
                .iter()
                .map(|name| format!(
                    "Undefined resource: '{name}'. Define it with 'let {name} = aws...'"
                ))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            undefined
                .iter()
                .map(|diagnostic| diagnostic.range.start.line)
                .collect::<Vec<_>>(),
            (0..names.len() as u32).collect::<Vec<_>>()
        );
    }
}
