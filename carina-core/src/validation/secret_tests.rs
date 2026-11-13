use super::*;
use crate::parser::{LoadedProviderContext, ProviderContext};
use crate::resource::{AuthoredDeferredValue, AuthoredValue, ExpandedDeferredValue, ExpandedValue};

fn config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

fn assert_safe(error: &(impl std::fmt::Display + std::fmt::Debug), text: &str) {
    assert!(!format!("{error}\n{error:?}").contains(text), "{error:?}");
}

#[test]
fn secret_origin_4275_directory_mismatch_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    for (name, source) in [
        ("providers.crn", "provider mock {}\n"),
        ("producer.crn", "let producer = mock.test.Secret {}\n"),
        (
            "main.crn",
            "mock.test.Secret { params = secret(\"private-${producer.text}\") }\n",
        ),
    ] {
        std::fs::write(dir.path().join(name), source).unwrap();
    }
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config())
        .unwrap()
        .into_parsed();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Secret")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "text",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "params",
                    crate::schema::PatternAttributeType::int(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let report = crate::parser::retained::judge_expressions(&parsed, &schemas, &config());
    let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let resource = tree
        .resources()
        .find(|r| r.attributes().contains_key("params"))
        .unwrap();
    let expression = report
        .expression(resource.attribute_value_location("params", &crate::schema::FieldPath::new()))
        .unwrap();
    let provider = LoadedProviderContext::empty_for_test();
    let index = BindingIndex::from_parsed(&parsed, &schemas, &provider);
    let sink = crate::schema::CompiledAttributeType::int();
    let owner = crate::parser::SchemaTypeOwner::Provider("mock".into());
    let authored = crate::parser::parse_file(
        &std::fs::read_to_string(dir.path().join("main.crn")).unwrap(),
        &dir.path().join("main.crn"),
        &config(),
    )
    .unwrap();
    let error = check_deferred_access_type(
        &authored.resources[0].attributes["params"],
        crate::schema::TypeInSchema::schemaless(&sink).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&owner),
        ),
        expression,
        &schemas,
        &index,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("expected Int, got String"),
        "{error}"
    );
    assert_safe(&error, "private-");
    let DeferredAccessTypeError::TypeMismatch { types, origin } = error else {
        panic!("expected mismatch")
    };
    assert_eq!(types.expected(), "Int");
    assert_eq!(types.actual(), "String");
    assert_eq!(origin, "from (secret)");
}

#[test]
fn secret_origin_4275_calls_pipes_closures_and_composition_wrap_immediately() {
    for expression in [
        "secret(\"private-${producer.text}\")",
        "\"private-${producer.text}\" |> secret()",
        "secret(secret(\"private-${producer.text}\"))",
    ] {
        let parsed = crate::parser::parse_and_resolve(&format!(
            "let producer = mock.test.Secret {{}}\nlet value = {expression}\n"
        ))
        .unwrap();
        let value = &parsed.variables["value"];
        assert!(
            !format!("{value:?}").contains("private-"),
            "{expression}: {value:?}"
        );
        let AuthoredValue::Deferred(AuthoredDeferredValue::Secret(inner)) = value else {
            panic!("{expression}: {value:?}")
        };
        assert!(!matches!(
            inner.expose_plaintext(),
            AuthoredValue::Deferred(AuthoredDeferredValue::Secret(_))
        ));
        assert!(
            !format!("{:?}", parsed.parser_binding_metadata.syntax.expressions())
                .contains("private-")
        );
    }
}

#[test]
fn secret_origin_4275_nested_builtin_errors_are_redacted() {
    for call in [
        "cidr_subnet(\"10.0.0.0/8\", 99, 1)",
        "cidr_subnet(\"private-invalid-cidr\", 1, 1)",
    ] {
        let error =
            crate::parser::parse_and_resolve(&format!("let value = secret({call})")).unwrap_err();
        assert_safe(&error, "10.0.0.0");
        assert_safe(&error, "private-");
        assert!(!format!("{error:?}").contains(call));
    }
}

#[test]
fn secret_origin_4275_constraint_rejects_secret() {
    let error = crate::parser::parse_and_resolve("arguments { password: String { validation { condition = secret(\"private-constraint\") == password } } }").unwrap_err();
    assert!(error.to_string().contains("constraint"), "{error}");
    assert_safe(&error, "private-");
}

#[test]
fn secret_origin_4275_deserialization_cannot_create_ordinary_secret_call() {
    assert!(crate::builtins::BuiltinFunction::from_name("secret").is_none());
    let wire = serde_json::json!({"FunctionCall": {"name": "secret", "args": [{"String": "private-wire"}]}});
    let mut ordinary = wire.clone();
    ordinary["FunctionCall"]["name"] = "upper".into();
    assert!(serde_json::from_value::<ExpandedValue>(ordinary.clone()).is_ok());
    assert!(serde_json::from_value::<ExpandedDeferredValue>(ordinary).is_ok());
    if let Ok(value) = serde_json::from_value::<ExpandedValue>(wire.clone()) {
        assert!(
            matches!(
                value,
                ExpandedValue::Deferred(ExpandedDeferredValue::Secret(_))
            ),
            "{value:?}"
        );
    }
    if let Ok(value) = serde_json::from_value::<ExpandedDeferredValue>(wire) {
        assert!(
            matches!(value, ExpandedDeferredValue::Secret(_)),
            "{value:?}"
        );
    }
}

#[test]
fn secret_origin_4275_deferred_for_template_is_already_secret() {
    let parsed = crate::parser::parse_and_resolve("let producer = mock.test.Secret {}\nlet items = for x in producer.text { mock.test.Secret { text = secret(\"private-${x}\") } }").unwrap();
    let crate::parser::DeferredForBody::Resource(template) =
        parsed.parsed_deferred_for_expressions()[0].body()
    else {
        panic!("expected template")
    };
    assert!(matches!(
        template.attributes().get("text"),
        Some(AuthoredValue::Deferred(AuthoredDeferredValue::Secret(_)))
    ));
    assert!(!format!("{template:?}").contains("private-"));
}

#[test]
fn secret_origin_4275_module_arguments_inside_secret_are_substituted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { input: String }\nattributes { value = secret(\"private-${input}\") }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.crn"), "let template = use { source = './child' }\nlet producer = mock.test.Secret {}\nlet child = template { input = producer.text }\n").unwrap();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config()).unwrap();
    let graph = deferred_types::with_session_for_test(&config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed.admit(&SchemaRegistry::new(), session),
            &path,
        )
    })
    .unwrap();
    let debug = format!("{graph:?}");
    assert!(!debug.contains("private-"));
}

#[test]
fn secret_origin_4275_if_condition_diagnostic_has_no_source_literal() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(dir.path().join("child/main.crn"), "arguments { flag: Bool }\nfn choose(flag: Bool, ignored: String): Bool { flag }\nlet hidden = if choose(flag, secret(\"private-if\")) { mock.test.Secret {} }\nattributes { value = hidden.text }\n").unwrap();
    std::fs::write(
        dir.path().join("main.crn"),
        "let template = use { source = './child' }\nlet child = template { flag = false }\n",
    )
    .unwrap();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config()).unwrap();
    let error = deferred_types::with_session_for_test(&config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed.admit(&SchemaRegistry::new(), session),
            &path,
        )
    })
    .unwrap_err();
    assert!(error.to_string().contains("dropped branch"), "{error}");
    assert_safe(&error, "private-");
}

#[test]
fn secret_origin_4275_retained_debug_hides_literal_keys_and_indices() {
    let parsed = crate::parser::parse_and_resolve(
        "arguments { input: String }\nlet value = secret({ 'private-key' = input })\n",
    )
    .unwrap();
    assert!(
        !format!("{:?}", parsed.parser_binding_metadata.syntax.expressions()).contains("private-")
    );
}

#[test]
fn secret_origin_4275_loop_source_never_retains_secret_literals() {
    for source in [
        "for x in [1, 2] { mock.test.Secret { text = secret(\"private-loop\") } }",
        "if true { mock.test.Secret { text = secret(\"private-branch\") } }",
        "for x in [1, 2] { mock.test.Secret { text = \"private-pipe\" |> secret() } }",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        assert!(!format!("{parsed:?}").contains("private-"), "{parsed:?}");
        for resource in &parsed.resources {
            let origin = resource.loop_provenance.display_origin().unwrap();
            assert!(origin.source().source_text().is_none());
            assert!(origin.source().source_span().is_some());
        }
    }
}

#[test]
fn secret_origin_4275_arithmetic_visitor_keeps_the_secret_boundary() {
    let parsed = crate::parser::parse_and_resolve("let producer = mock.test.Secret {}\nmock.test.Secret { text = secret(length(\"private-${producer.text}\") + 1) }\n").unwrap();
    let schemas = SchemaRegistry::new();
    let report = crate::parser::retained::judge_expressions(&parsed, &schemas, &config());
    let provider = LoadedProviderContext::empty_for_test();
    let index = BindingIndex::from_parsed(&parsed, &schemas, &provider);
    let findings = check_integer_arithmetic_types_with_sink(
        &parsed.resources[1].attributes["text"],
        Some(RefSink::TypeExpr(&TypeExpr::String)),
        &report,
        &index,
    );
    assert!(!findings.is_empty(), "the Int/String mismatch must survive");
    assert!(
        !format!("{findings:?}").contains("private-"),
        "{findings:?}"
    );
    for finding in findings {
        assert_safe(&finding, "private-");
    }
}

#[test]
fn secret_origin_4275_resolver_access_errors_drop_inner_payloads() {
    use crate::resource::{
        AccessMissPolicy, ConcreteValue, DeferredValue, FunctionArgument, PathSegment, Subscript,
        Value,
    };
    let call: ExpandedValue = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::split,
        args: vec!["-", "private-access"]
            .into_iter()
            .map(|text| {
                FunctionArgument::from_value(Value::Concrete(ConcreteValue::String(text.into())))
            })
            .collect(),
        source_line: 1.into(),
    });
    let access = crate::resource::apply_postfix_access(
        call,
        PathSegment::Subscript {
            index: Subscript::Int { index: 99 },
        },
        1,
        AccessMissPolicy::Error,
    )
    .unwrap();
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(access).into()));
    let error = deferred_types::with_session_for_test(&config(), |session| {
        crate::resolver::resolve_ref_value(
            &secret,
            &crate::binding_index::ResolvedBindings::default(),
            session,
        )
    })
    .unwrap_err();
    assert_safe(&error, "private-");
    assert!(matches!(
        error,
        crate::resolver::ResolutionError::InsideSecret { .. }
    ));
}

#[test]
fn secret_origin_4275_providerless_deferral_has_no_plaintext() {
    let parsed =
        crate::parser::parse_and_resolve("let value = secret(\"private-deferral\")").unwrap();
    let config = config();
    let report =
        crate::parser::retained::judge_expressions(&parsed, &SchemaRegistry::new(), &config);
    let expression = report
        .binding(&crate::resource::RelativeBindingName::new("value"))
        .unwrap();
    let expected = crate::parser::parse_type_expr_str("ProviderOnlyType", &config).unwrap();
    let check = typecheck::check_type_expr_value_providerless(
        &expected,
        &parsed.variables["value"],
        &config,
        expression,
    );
    assert!(!format!("{check:?}").contains("private-"));
    let error = check.require_provider_schemas().unwrap_err();
    assert_safe(&error, "private-");
}

#[test]
fn secret_origin_4275_deferred_for_header_uses_wrapped_values() {
    let parsed = crate::parser::parse_and_resolve("arguments { input: Bool }\nfor item in (if input { secret([\"private-header\"]) } else { [] }) { mock.test.Secret { text = item } }").unwrap();
    assert!(!format!("{parsed:?}").contains("private-"), "{parsed:?}");
}

#[test]
fn secret_origin_4275_reference_validation_drops_inner_paths() {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };
    let mut parsed = crate::parser::parse_and_resolve("let producer = mock.test.Secret {}\nmock.test.Secret { params = secret(producer.config.private_field) }").unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Secret")
            .attribute(
                PatternAttributeSchema::optional("params", PatternAttributeType::int()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "config",
                    PatternAttributeType::struct_(
                        "Config",
                        vec![
                            PatternStructField::optional("known", PatternAttributeType::string())
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    crate::parser::retained::judge_configuration(&mut parsed, &schemas, &config());
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config());
    let provider = LoadedProviderContext::empty_for_test();
    let index = BindingIndex::from_parsed(&parsed, &schemas, &provider);
    let error =
        validate_resource_ref_types(loaded.judged(), &HashSet::new(), &index, &mut Vec::new())
            .unwrap_err();
    assert_safe(&error, "private_field");
}

#[test]
fn secret_origin_4275_deferred_for_evaluates_secret_inner_calls_safely() {
    let parsed = crate::parser::parse_and_resolve("let producer = mock.test.Secret {}\nlet items = for x in producer.text { mock.test.Secret { text = secret(upper(\"private-template\")) } }").unwrap();
    let crate::parser::DeferredForBody::Resource(template) =
        parsed.parsed_deferred_for_expressions()[0].body()
    else {
        panic!("expected resource template")
    };
    let Some(AuthoredValue::Deferred(AuthoredDeferredValue::Secret(inner))) =
        template.attributes().get("text")
    else {
        panic!("expected secret")
    };
    assert!(
        matches!(inner.expose_plaintext(), AuthoredValue::Concrete(crate::resource::ConcreteValue::String(text)) if text == "PRIVATE-TEMPLATE")
    );
    assert!(!format!("{parsed:?}").contains("PRIVATE-TEMPLATE"));
    let error = crate::parser::parse_and_resolve("let producer = mock.test.Secret {}\nlet items = for x in producer.text { mock.test.Secret { text = secret(cidr_subnet(\"private-template-cidr\", 1, 1)) } }").unwrap_err();
    assert_safe(&error, "private-");
}

#[test]
fn secret_origin_4275_module_builtin_failure_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { input: String }\nattributes { value = secret(cidr_subnet(input, 1, 1)) }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("main.crn"), "let template = use { source = './child' }\nlet child = template { input = 'private-module-cidr' }\n").unwrap();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &config()).unwrap();
    let error = deferred_types::with_session_for_test(&config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed.admit(&SchemaRegistry::new(), session),
            &path,
        )
    })
    .unwrap_err();
    assert_safe(&error, "private-");
}

#[test]
fn secret_origin_4275_deserialization_errors_do_not_decode_secret_arguments() {
    for wire in [
        serde_json::json!({"FunctionCall": {"name": "secret", "args": [{"Int": "private-wire"}]}}),
        serde_json::json!({"FunctionCall": {"name": "secret", "args": "private-wire"}}),
        serde_json::json!({"Secret": {"Int": "private-wire"}}),
    ] {
        let error = serde_json::from_value::<ExpandedValue>(wire.clone()).unwrap_err();
        assert_safe(&error, "private-");
        let error = serde_json::from_value::<ExpandedDeferredValue>(wire).unwrap_err();
        assert_safe(&error, "private-");
    }
}

#[test]
fn secret_origin_4275_intrinsic_can_be_shadowed_by_an_ordinary_closure() {
    for expression in ["secret(\"private-a\")", "\"private-a\" |> secret()"] {
        let parsed = crate::parser::parse_and_resolve(&format!(
            "let secret = replace(\"-\", \"_\")\nlet value = {expression}\n"
        ))
        .unwrap();
        assert_eq!(
            parsed.variables["value"],
            AuthoredValue::Concrete(crate::resource::ConcreteValue::String("private_a".into()))
        );
    }
}

#[test]
fn secret_origin_4275_pipe_argument_failures_are_protected() {
    for source in [
        "let value = \"input\" |> secret(cidr_subnet(\"private-pipe-cidr\", 1, 1))",
        "let value = (\"input\" |> secret(cidr_subnet(\"private-pipe-cidr\", 1, 1)))",
    ] {
        let error = crate::parser::parse_and_resolve(source).unwrap_err();
        assert_safe(&error, "private-");
    }
}

#[test]
fn secret_origin_4275_unknown_attribute_keeps_public_context() {
    assert_secret_reference_context("producer.nonexistent", "unknown attribute");
}

#[test]
fn secret_origin_4275_reference_mismatch_keeps_public_context() {
    assert_secret_reference_context("producer.text", "expected Int, got String");
}

fn assert_secret_reference_context(reference: &str, expected: &str) {
    let mut parsed = crate::parser::parse_and_resolve(&format!(
        "let producer = mock.test.Secret {{}}\nmock.test.Secret {{ params = secret({reference}) }}"
    ))
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Secret")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "params",
                    crate::schema::PatternAttributeType::int(),
                )
                .build(),
            )
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "text",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    crate::parser::retained::judge_configuration(&mut parsed, &schemas, &config());
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config());
    let provider = LoadedProviderContext::empty_for_test();
    let index = BindingIndex::from_parsed(&parsed, &schemas, &provider);
    let error =
        validate_resource_ref_types(loaded.judged(), &HashSet::new(), &index, &mut Vec::new())
            .unwrap_err()
            .to_string();
    assert!(
        error.contains("mock.test.Secret") && error.contains("params") && error.contains(expected),
        "{reference}: {error}"
    );
    assert!(!error.contains(reference), "{error}");
    assert!(!error.contains("nonexistent"), "{error}");
}

#[test]
fn secret_origin_4275_deserializer_normalizes_secret_depth() {
    let wire = serde_json::json!({"Secret": {"Secret": {"String": "private-depth"}}});
    let value: ExpandedValue = serde_json::from_value(wire).unwrap();
    let ExpandedValue::Deferred(ExpandedDeferredValue::Secret(inner)) = value else {
        panic!("expected secret")
    };
    assert!(matches!(
        inner.expose_plaintext(),
        ExpandedValue::Concrete(crate::resource::ConcreteValue::String(_))
    ));
}

#[test]
fn secret_origin_4275_non_pest_syntax_error_drops_argument_excerpt() {
    let source = "private-argument";
    let error = crate::parser::SourceSpanIndex::parse(source)
        .unwrap_err()
        .inside_secret_at(7);
    assert_safe(&error, source);
}
