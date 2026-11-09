//! Directory-level contracts for the authored expression judgement. These pin
//! diagnostics and inferred exports as well as acceptance of deferred values.

use carina_core::{
    EnvironmentSnapshot,
    config_loader::parse_directory,
    parser::{ParsedFile, ProviderContext},
    paths::ResolvedPath,
    schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
    },
    static_type::{ExportStaticType, InferenceError, InferredUncheckedReason, StaticType},
};

fn directory(files: &[(&str, &str)]) -> ParsedFile {
    let root = tempfile::tempdir().unwrap();
    for (name, source) in files {
        std::fs::write(root.path().join(name), source).unwrap();
    }
    parse_directory(
        &ResolvedPath::new(root.path()).unwrap(),
        &ProviderContext::new(EnvironmentSnapshot::empty()),
    )
    .expect("the directory must parse before schema-aware judgement")
    .into_parsed()
}

fn source_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Source")
            .attribute(AttributeSchema::provider_populated("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated(
                    "names",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "role_name",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_schema_type(
                            "mock",
                            "test.Source",
                            "Name",
                        )),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

fn findings(parsed: &ParsedFile, schemas: &SchemaRegistry) -> Vec<String> {
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let loaded = carina_core::config_loader::load_buffer_configuration(parsed, schemas, &context);
    loaded
        .judged()
        .report()
        .findings()
        .iter()
        .map(|finding| finding.detail().to_string())
        .collect()
}

#[test]
fn directory_function_argument_diagnostic_keeps_parameter_context() {
    let parsed = directory(&[
        ("source.crn", "let source = mock.test.Source {}\n"),
        (
            "functions.crn",
            "fn consume(number: Int): String { 'fixed' }\n",
        ),
        ("main.crn", "let result = consume(source.name)\n"),
    ]);
    let messages = findings(&parsed, &source_schemas());
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("function 'consume': parameter 'number'")
            && messages[0].contains("expected Int")
            && messages[0].contains("got String"),
        "function argument lost its diagnostic context: {messages:?}"
    );
}

#[test]
fn directory_function_return_diagnostic_keeps_function_context() {
    let parsed = directory(&[
        ("source.crn", "let source = mock.test.Source {}\n"),
        ("functions.crn", "fn consume(): Int { source.name }\n"),
        ("main.crn", "let result = consume()\n"),
    ]);
    let messages = findings(&parsed, &source_schemas());
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("function 'consume': return value")
            && messages[0].contains("expected Int")
            && messages[0].contains("got String"),
        "function return lost its diagnostic context: {messages:?}"
    );
}

#[test]
fn directory_unloaded_function_annotation_defers_literal_assignment() {
    let parsed = directory(&[
        (
            "functions.crn",
            "fn consume(value: Arn): String { 'fixed' }\n",
        ),
        ("main.crn", "let result = consume('arn:example')\n"),
    ]);
    let messages = findings(&parsed, &source_schemas());
    assert!(
        messages.is_empty(),
        "unloaded annotation must defer: {messages:?}"
    );
}

#[test]
fn directory_dynamic_lookup_export_requires_annotation_without_losing_declared_types() {
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    for (annotation, declared) in [("", false), (": String", true)] {
        let parsed = directory(&[
            ("values.crn", "let values = { name = 'carina' }\n"),
            (
                "exports.crn",
                &format!("exports {{ result{annotation} = lookup(values, 'name', 'default') }}\n"),
            ),
        ]);
        let inferred = carina_core::validation::inference::apply_inference(
            parsed,
            &SchemaRegistry::new(),
            &context,
        );
        let ty = &inferred.export_params[0].static_type;
        if declared {
            assert_eq!(
                *ty,
                ExportStaticType::Declared(carina_core::parser::TypeExpr::String)
            );
        } else {
            assert!(
                matches!(
                    ty,
                    ExportStaticType::Inferred(StaticType::Unchecked(
                        InferredUncheckedReason::InferenceFailed(
                            InferenceError::UnknownType { .. }
                        )
                    ))
                ),
                "dynamic lookup must require an export annotation: {ty:?}"
            );
        }
    }
}

fn assert_loop_accepted(source: &str) {
    // Keep the iterable in a sibling file: the directory loader, rather than
    // a single-file parse, must establish the loop slots' evidence.
    let parsed = directory(&[
        (
            "source.crn",
            "let source = mock.test.Source {}\nlet roles = { admin = true }\n",
        ),
        ("main.crn", source),
    ]);
    let messages = findings(&parsed, &source_schemas());
    assert!(
        messages.is_empty(),
        "bare loop slot must remain accepted: {messages:?}"
    );
}

#[test]
fn directory_loop_map_keys_remain_unannotated_at_refined_sinks() {
    assert_loop_accepted("for key, _ in roles { mock.test.Source { role_name = key } }\n");
}

#[test]
fn directory_loop_schema_list_elements_remain_unannotated_at_refined_sinks() {
    assert_loop_accepted("for name in source.names { mock.test.Source { role_name = name } }\n");
}

fn assert_sink_accepted(sink: ResourceSchema, value: &str) {
    let mut schemas = source_schemas();
    schemas.insert("mock", sink.validate().unwrap());
    let parsed = directory(&[
        ("source.crn", "let source = mock.test.Source {}\n"),
        (
            "main.crn",
            &format!("mock.test.Sink {{ target = {value} }}\n"),
        ),
    ]);
    let messages = findings(&parsed, &schemas);
    assert!(
        messages.is_empty(),
        "schema sink must accept deferred members: {messages:?}"
    );
}

fn strings_or_string_list() -> AttributeType {
    AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::list(AttributeType::string()),
    ])
}

#[test]
fn directory_union_list_sink_accepts_a_nonfirst_member() {
    assert_sink_accepted(
        ResourceSchema::new("test.Sink")
            .attribute(AttributeSchema::required("target", strings_or_string_list()).build()),
        r#"["${source.name}-root"]"#,
    );
}

#[test]
fn directory_struct_ref_sink_preserves_nested_field_types() {
    assert_sink_accepted(
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::required(
                    "target",
                    AttributeType::struct_(
                        "Document",
                        vec![StructField::required("body", AttributeType::ref_("Body")).build()],
                    ),
                )
                .build(),
            )
            .with_def(
                "Body",
                AttributeType::struct_(
                    "Body",
                    vec![
                        StructField::required(
                            "names",
                            AttributeType::list(AttributeType::string()),
                        )
                        .build(),
                    ],
                ),
            ),
        r#"{ body = { names = ["${source.name}-root"] } }"#,
    );
}

#[test]
fn directory_map_union_sink_accepts_different_member_shapes() {
    assert_sink_accepted(
        ResourceSchema::new("test.Sink").attribute(
            AttributeSchema::required("target", AttributeType::map(strings_or_string_list()))
                .build(),
        ),
        r#"{ first = ["${source.name}-first"] second = "${source.name}-second" }"#,
    );
}
