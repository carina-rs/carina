use super::*;
use crate::parser::{ParsedFile, ProviderContext};

fn config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

fn directory(source: &str, sibling: &str) -> (tempfile::TempDir, crate::paths::ResolvedPath) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.crn"), source).unwrap();
    std::fs::write(dir.path().join("source.crn"), sibling).unwrap();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    (dir, path)
}

fn parsed_directory(source: &str) -> ParsedFile {
    let (_dir, path) = directory(source, "let src = mock.test.Source {}\n");
    crate::config_loader::parse_directory(&path, &config())
        .unwrap()
        .into_parsed()
}

#[test]
fn round1_export_interpolation_checks_every_non_string_sink() {
    for sink in ["Int", "Bool", "list(String)"] {
        let parsed = parsed_directory(&format!("exports {{ port: {sink} = \"${{src.name}}\" }}"));
        let bindings = crate::binding_index::ResolvedBindings::for_authored_validation(&parsed);
        let result = deferred_types::with_session_for_test(&config(), |session| {
            check_export_values_pre_apply_providerless(&parsed.export_params, &bindings, session)
        });
        assert!(
            result.is_err(),
            "deferred interpolation admitted into {sink}"
        );
    }
}

#[test]
fn round1_attribute_interpolation_checks_every_non_string_sink() {
    for sink in ["Int", "Bool", "list(String)"] {
        let parsed = parsed_directory(&format!(
            "attributes {{ port: {sink} = \"${{src.name}}\" }}"
        ));
        let result = deferred_types::with_session_for_test(&config(), |session| {
            validate_attribute_params_with_context(&parsed.attribute_params, session)
        });
        assert!(
            result.is_err(),
            "deferred interpolation admitted into {sink}"
        );
    }
}

#[test]
fn round1_function_argument_interpolation_is_checked_locally() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: Int): Int { n }\nlet src = mock.test.Source {}\nlet result = f(\"${src.name}\")",
    );
    assert!(
        result.is_err(),
        "authored function argument bypassed the Int sink"
    );
}

#[test]
fn round1_function_argument_interpolation_is_checked_across_files() {
    let (_dir, path) = directory(
        "let src = mock.test.Source {}\nlet result = f(\"${src.name}\")",
        "fn f(n: Int): Int { n }",
    );
    let result = crate::config_loader::parse_directory(&path, &config());
    assert!(
        result.is_err(),
        "sibling function argument bypassed the Int sink"
    );
}

#[test]
fn round1_function_return_interpolation_is_checked_across_files() {
    let (_dir, path) = directory(
        "let src = mock.test.Source {}\nlet result = f(src.name)",
        "fn f(n: String): Int { \"${n}\" }",
    );
    let result = crate::config_loader::parse_directory(&path, &config());
    assert!(
        result.is_err(),
        "sibling function return bypassed the Int sink"
    );
}

#[test]
fn round1_function_return_interpolation_is_checked_locally() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: String): Int { \"${n}\" }\nlet src = mock.test.Source {}\nlet result = f(src.name)",
    );
    assert!(
        result.is_err(),
        "authored function return bypassed the Int sink"
    );
}

#[test]
fn round1_concrete_function_arguments_keep_literal_precision() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: 'dev' | 'prod'): String { n }\nlet result = f('prod')",
    );
    assert!(
        result.is_ok(),
        "a valid literal was widened before its constraint check: {result:?}"
    );
}

#[test]
fn round1_folded_children_keep_every_failure() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    for source in [
        "let a = 'x'\nlet b = a ?? [1, 'y']",
        "let r = mock.test.Source {}\nlet a = 'x'\nlet b = a ?? r.no_such_attr",
        "let a = 'x'\nlet b = length([a ?? [1, 'y']])",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config());
        let judged = loaded.judged();
        assert_eq!(
            judged.report().findings().len(),
            1,
            "{source}: {:?}",
            judged.report().findings()
        );
        assert!(matches!(
            judged
                .report()
                .binding(&RelativeBindingName::new("b"))
                .unwrap()
                .static_type(),
            Err(crate::static_type::InferenceIssue::Failed(_))
        ));
    }
}

#[test]
fn round1_resource_sink_does_not_repeat_child_findings() {
    let parsed = parsed_directory("let sink = mock.test.Source { name = \"${src.no_such_attr}\" }");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config());
    assert_eq!(
        loaded.judged().report().findings().len(),
        1,
        "the child owns its failure: {:?}",
        loaded.judged().report().findings()
    );
}

#[test]
fn round1_deferred_loop_keys_and_indexes_are_not_concrete() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "tags",
                    crate::schema::PatternAttributeType::map(
                        crate::schema::PatternAttributeType::string(),
                    ),
                )
                .build(),
            )
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "names",
                    crate::schema::PatternAttributeType::list(
                        crate::schema::PatternAttributeType::string(),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Sink")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::refined_string(
                        Some(crate::schema::TypeIdentity::new(
                            Some("mock"),
                            ["test", "Sink"],
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
    for pattern in ["for k, v in src.tags", "for (i, v) in src.names"] {
        let name = if pattern.contains("k,") { "k" } else { "i" };
        let parsed = crate::parser::parse_and_resolve(&format!("let src = mock.test.Source {{}}\n{pattern} {{ mock.test.Sink {{ name = \"${{{name}}}\" }} }}")).unwrap();
        let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config());
        let judged = loaded.judged();
        assert!(
            judged.report().findings().is_empty(),
            "{pattern}: {:?}",
            judged.report().findings()
        );
        let tree = crate::parser::AnalysisTree::try_from_file(loaded.parsed()).unwrap();
        let resource = tree
            .resources()
            .find(|r| r.id().resource_type == "test.Sink")
            .unwrap();
        let expression = judged
            .report()
            .expression(resource.attribute_value_location("name", &crate::schema::FieldPath::new()))
            .unwrap();
        assert_eq!(
            crate::parser::retained::admit_expression(expression, &TypeExpr::String),
            Ok(crate::parser::retained::ExpressionAdmission::DeferredInterpolation),
            "{pattern}: {expression:?}"
        );
    }
}

#[test]
fn round1_module_argument_interpolation_cannot_bypass_substitution() {
    let (dir, path) = directory(
        "let template = use { source = './child' }\nlet child = template { n = \"${src.name}\" }",
        "let src = mock.test.Source {}",
    );
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { n: Int }\nattributes { value = n }",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config()).unwrap();
    let result = deferred_types::with_session_for_test(&config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            &path,
            session,
            &SchemaRegistry::new(),
        )
    });
    assert!(
        result.is_err(),
        "module argument interpolation bypassed substitution checks"
    );
}

#[test]
fn round1_instances_retain_each_authored_location() {
    let (dir, path) = directory(
        "let template = use { source = './child' }\nlet a = template { value = 42 }\nlet b = template { value = 'text' }",
        "",
    );
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { value: Int | String }\nattributes { result = value }",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config()).unwrap();
    let child = crate::parser::parse_file(
        "arguments { value: Int | String }\nattributes { result = value }",
        &dir.path().join("child/main.crn"),
        &config(),
    )
    .unwrap();
    let location = child.attribute_params[0].value_location().unwrap().clone();
    let graph = deferred_types::with_session_for_test(&config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            &path,
            session,
            &SchemaRegistry::new(),
        )
    })
    .unwrap();
    let types = graph.expression_report().types_at_for_test(&location);
    assert_eq!(
        types.len(),
        2,
        "one instance overwrote the other: {types:?}"
    );
    assert!(types.contains(&TypeExpr::Int), "{types:?}");
    assert!(types.contains(&TypeExpr::String), "{types:?}");
}

#[test]
fn round1_schema_bundle_replaces_findings_from_a_different_registry() {
    fn schemas(ty: crate::schema::PatternAttributeType) -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            crate::schema::PatternResourceSchema::new("test.Source")
                .attribute(crate::schema::PatternAttributeSchema::optional("name", ty).build())
                .validate()
                .unwrap(),
        );
        schemas
    }
    let parsed = crate::parser::parse_and_resolve(
        "let src = mock.test.Source {}\nlet value = src.name ?? 42",
    )
    .unwrap();
    let strings = schemas(crate::schema::PatternAttributeType::string());
    let integers = schemas(crate::schema::PatternAttributeType::int());
    let first = crate::config_loader::load_buffer_configuration(&parsed, &strings, &config());
    assert_eq!(first.judged().report().findings().len(), 1);
    let second =
        crate::config_loader::load_buffer_configuration(first.parsed(), &integers, &config());
    assert!(
        second.judged().report().findings().is_empty(),
        "{:?}",
        second.judged().report().findings()
    );
    assert_eq!(
        second
            .judged()
            .report()
            .binding(&RelativeBindingName::new("value"))
            .unwrap()
            .static_type(),
        Ok(&TypeExpr::Int)
    );
    assert_eq!(
        first.judged().report().findings().len(),
        1,
        "the first bundle is immutable"
    );
}
