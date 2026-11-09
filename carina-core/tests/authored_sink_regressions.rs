//! Directory-level contracts for the authored expression judgement. These pin
//! diagnostics and inferred exports as well as acceptance of deferred values.

use carina_core::{
    EnvironmentSnapshot,
    binding_index::BindingIndex,
    config_loader::parse_directory,
    parser::{ParsedFile, ProviderContext},
    paths::ResolvedPath,
    schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
    },
    static_type::{ExportStaticType, InferenceError, InferredUncheckedReason, StaticType},
    validation::{
        ResourceRefTypeValidationError, ValidationTypeContext, check_bare_binding_uses,
        validate_resource_admissions,
    },
};
use std::collections::HashSet;

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

fn validation_findings(parsed: &ParsedFile, schemas: &SchemaRegistry) -> Vec<String> {
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let loaded = carina_core::config_loader::load_buffer_configuration(parsed, schemas, &context);
    let messages = loaded
        .judged()
        .report()
        .findings()
        .iter()
        .map(|finding| finding.detail().to_string())
        .collect::<Vec<_>>();
    if !messages.is_empty() {
        return messages;
    }
    let signatures = Default::default();
    let validation =
        ValidationTypeContext::new_providerless(loaded.judged(), &context, &signatures);
    let uses = check_bare_binding_uses(&validation);
    let messages = uses
        .findings()
        .iter()
        .map(|finding| finding.error.to_string())
        .collect::<Vec<_>>();
    if !messages.is_empty() {
        return messages;
    }
    let bindings = BindingIndex::from_parsed(loaded.parsed(), schemas, &context);
    // Follow the validation stages in order. Concrete values and binding-owned
    // loop slots need their owning checks beyond the expression-only report.
    match validate_resource_admissions(
        loaded.judged(),
        &bindings,
        &HashSet::from(["mock".to_string()]),
        &mut Vec::new(),
    ) {
        Ok(()) => Vec::new(),
        Err(ResourceRefTypeValidationError::Validation(messages)) => {
            messages.lines().map(str::to_string).collect()
        }
        Err(error) => panic!("the fixture must have valid analysis: {error}"),
    }
}

fn assert_single_finding(messages: &[String], expected: &[&str]) {
    assert_eq!(messages.len(), 1, "{messages:?}");
    for text in expected {
        assert!(messages[0].contains(text), "missing {text:?}: {messages:?}");
    }
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

    // An unloaded annotation still rejects a Bool literal.
    let parsed = directory(&[
        (
            "functions.crn",
            "fn consume(value: Arn): String { 'fixed' }\n",
        ),
        ("main.crn", "let result = consume(true)\n"),
    ]);
    assert_single_finding(
        &findings(&parsed, &source_schemas()),
        &[
            "function 'consume': parameter 'value'",
            "expected Arn, got Bool",
        ],
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

fn assert_loop_accepted(source: &str, incompatible_source: &str) {
    // Keep the iterable in a sibling file: the directory loader, rather than
    // a single-file parse, must establish the loop slots' evidence.
    let fixture = |main| {
        directory(&[
            (
                "source.crn",
                "let source = mock.test.Source {}\nlet roles = { admin = true }\n",
            ),
            ("main.crn", main),
        ])
    };
    let parsed = fixture(source);
    let messages = findings(&parsed, &source_schemas());
    assert!(
        messages.is_empty(),
        "bare loop slot must remain accepted: {messages:?}"
    );

    let mut schemas = source_schemas();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(AttributeSchema::required("target", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let messages = validation_findings(&fixture(incompatible_source), &schemas);
    assert!(
        !messages.is_empty(),
        "loop slot must be judged: {messages:?}"
    );
    // Duplicate loop-body findings are a known separate issue; do not pin their count.
    assert!(
        messages
            .iter()
            .all(|message| message.contains("expected Int, got String")),
        "{messages:?}"
    );
}

#[test]
fn directory_loop_map_keys_remain_unannotated_at_refined_sinks() {
    assert_loop_accepted(
        "for key, _ in roles { mock.test.Source { role_name = key } }\n",
        "for key, _ in roles { mock.test.Sink { target = key } }\n",
    );
}

#[test]
fn directory_loop_schema_list_elements_remain_unannotated_at_refined_sinks() {
    assert_loop_accepted(
        "for name in source.names { mock.test.Source { role_name = name } }\n",
        "for name in source.names { mock.test.Sink { target = name } }\n",
    );
}

fn assert_sink_accepted(sink: ResourceSchema, value: &str, expected_rejection: &str) {
    let mut schemas = source_schemas();
    schemas.insert("mock", sink.validate().unwrap());
    let fixture = |value| {
        directory(&[
            ("source.crn", "let source = mock.test.Source {}\n"),
            (
                "main.crn",
                &format!("mock.test.Sink {{ target = {value} }}\n"),
            ),
        ])
    };
    let parsed = fixture(value);
    let messages = findings(&parsed, &schemas);
    assert!(
        messages.is_empty(),
        "schema sink must accept deferred members: {messages:?}"
    );
    let messages = validation_findings(&parsed, &schemas);
    assert!(messages.is_empty(), "{messages:?}");
    assert_single_finding(
        &validation_findings(&fixture("1"), &schemas),
        &["mock.test.Sink", expected_rejection],
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
        "expected String | List<String>, got Int",
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
        "expected Struct(Document), got Int",
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
        "expected Map<String | List<String>>, got Int",
    );
}
