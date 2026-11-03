use super::*;
use std::path::Path;

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}
fn slice<'a>(source: &'a str, location: &DeclarationLocation) -> &'a str {
    &source[location.span().start_byte()..location.span().end_byte()]
}

#[test]
fn generated_prefix_names_retain_authored_provenance() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let source = "let bucket = mock.test.Resource { name_prefix = 'service-' }\n";
    let mut parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    crate::identifier::resolve_attr_prefixes(&mut parsed.resources, &schemas).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    let resource = tree.resources().next().unwrap();
    assert_eq!(
        slice(source, resource.attribute_key_location("name")),
        "name_prefix"
    );
}

#[test]
fn canonical_block_names_retain_nested_and_deferred_provenance() {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    };
    let source = "let src = mock.test.Resource {}\nlet direct = mock.test.Resource { rule { child { value = 'bad' } } }\nlet items = for x in src.items { mock.test.Resource { rule { child { value = x } } } }\nlet assigned = mock.test.Resource { rules = [{ child = [{ value = 'bad' }] }] }\n";
    let mut parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Resource")
            .attribute(
                AttributeSchema::optional(
                    "rules",
                    AttributeType::list(AttributeType::struct_(
                        "Rule",
                        vec![
                            StructField::optional(
                                "children",
                                AttributeType::list(AttributeType::struct_(
                                    "Child",
                                    vec![
                                        StructField::optional("value", AttributeType::string())
                                            .build(),
                                    ],
                                )),
                            )
                            .with_block_name("child")
                            .build(),
                        ],
                    )),
                )
                .with_block_name("rule")
                .build(),
            )
            .validate()
            .unwrap(),
    );
    crate::schema::resolve_file_block_names_with_schemas(&mut parsed, &schemas).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    for resource in tree
        .resources()
        .filter(|item| item.binding() != Some("src"))
    {
        assert_eq!(
            slice(source, resource.attribute_key_location("rules")),
            if resource.binding() == Some("assigned") {
                "rules"
            } else {
                "rule"
            }
        );
        let key = resource.attribute_keys().iter().find(|key|
            matches!(key.path(), [AttributePathSegment::Attribute(root), AttributePathSegment::ListIndex(0), AttributePathSegment::StructField(child) | AttributePathSegment::Key(child)] if root == "rules" && child == "children")
        ).expect("canonical nested key");
        assert_eq!(slice(source, key.location()), "child");
        let value_path = crate::schema::FieldPath::new()
            .push_index(0)
            .push_field("children")
            .push_index(0)
            .push_field("value");
        assert_eq!(
            slice(
                source,
                resource.attribute_value_location("rules", &value_path)
            ),
            if matches!(resource.binding(), Some("direct" | "assigned")) {
                "'bad'"
            } else {
                "x"
            }
        );
    }
}

#[test]
fn resolution_preserves_values_and_locations() {
    let config = context();
    let parsed = parse_file(
        "let holder = mock.test.Resource { name = target.name }\nlet target = mock.test.Resource { name = 'ready' }\n",
        Path::new("sibling.crn"),
        &config,
    )
    .unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    crate::validation::deferred_types::with_session_for_test(&config, |session| {
        let resolved = resolve_resource_refs_from_analysis(&tree, session).unwrap();
        assert_eq!(
            resolved.resources[0].attributes["name"],
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                "ready".into()
            ))
        );
        let resolved_tree = AnalysisTree::try_from_file(&resolved).unwrap();
        assert_eq!(
            resolved_tree
                .resources()
                .map(|item| item.location())
                .collect::<Vec<_>>(),
            tree.resources()
                .map(|item| item.location())
                .collect::<Vec<_>>()
        );
    });
}

#[test]
fn type_resolution_preserves_parameter_types() {
    let config = context();
    let source = "arguments { value: String count: Int = 1 }\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &config).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    crate::validation::deferred_types::with_session_for_test(&config, |session| {
        let (resolved, report) =
            crate::validation::resolve_file_type_exprs_from_analysis(&tree, session, None);
        assert!(
            report.diagnostics().is_empty(),
            "{:#?}",
            report.diagnostics()
        );
        assert_eq!(resolved.arguments.len(), 2);
        assert_eq!(resolved.arguments[0].name, "value");
        assert_eq!(resolved.arguments[0].static_type, TypeExpr::String);
        assert_eq!(resolved.arguments[1].name, "count");
        assert_eq!(resolved.arguments[1].static_type, TypeExpr::Int);
        assert_eq!(
            resolved.arguments[1].default,
            Some(crate::resource::Value::Concrete(
                crate::resource::ConcreteValue::Int(1)
            ))
        );
    });
}

#[test]
fn nested_keys_and_repeated_blocks_keep_occurrence_paths()
-> Result<(), crate::parser::AnalysisTreeError> {
    let source = "mock.test.Resource {\n  settings = { inner = [{ label = 'a' }, { label = 'b' }] }\n  rule { name = 'first' }\n  rule { name = 'second' }\n}\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed)?;
    let resource = tree.resources().next().unwrap();
    let keys = resource.attribute_keys();
    assert_eq!(
        keys.iter()
            .map(|key| slice(source, key.location()))
            .collect::<Vec<_>>(),
        [
            "settings", "inner", "label", "label", "rule", "name", "rule", "name"
        ]
    );
    use AttributePathSegment::{Attribute, Key, ListIndex, StructField};
    let path = |parts: &[AttributePathSegment]| parts.to_vec();
    assert_eq!(
        keys[3].path(),
        path(&[
            Attribute("settings".into()),
            Key("inner".into()),
            ListIndex(1),
            Key("label".into())
        ])
    );
    assert_eq!(
        keys[5].path(),
        path(&[
            Attribute("rule".into()),
            ListIndex(0),
            StructField("name".into())
        ])
    );
    assert_eq!(
        keys[7].path(),
        path(&[
            Attribute("rule".into()),
            ListIndex(1),
            StructField("name".into())
        ])
    );
    assert_ne!(keys[5].location(), keys[7].location());
    Ok(())
}

#[test]
fn reference_occurrences_and_prefixes_preserve_interpolation_postfix_and_heredocs()
-> Result<(), crate::parser::AnalysisTreeError> {
    let source = "let target = mock.test.Resource {}\nlet holder = mock.test.Resource {\n  note = <<-EOT\n    😀 unicode\n  EOT\n  values = [target.items[0].name, (target.items)[1].name, \"${target.items[2].name}\", target.items.123]\n}\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed)?;
    let holder = tree
        .resources()
        .find(|resource| resource.binding() == Some("holder"))
        .unwrap();
    let refs = holder.references();
    assert_eq!(refs.len(), 4);
    assert_eq!(
        refs.iter()
            .map(|reference| slice(source, reference.location()))
            .collect::<Vec<_>>(),
        [
            "target.items[0].name",
            "(target.items)[1].name",
            "target.items[2].name",
            "target.items.123"
        ]
    );
    assert!(
        refs.iter()
            .all(|reference| slice(source, reference.prefix_location()) == "target.items")
    );
    assert!(
        refs.iter()
            .all(|reference| reference.location().file().path() == Path::new("sibling.crn"))
    );
    Ok(())
}

#[test]
fn inventory_preserves_all_kinds_export_names_and_upstream_source_values() {
    let source = "provider mock {}\narguments { input: String }\nattributes { output: String = input }\nexports { same: String = 'first' same = 'second' }\nlet state = upstream_state { source = '../upstream' }\nfn identity(x: String): String { x }\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let inventory = parsed.parser_binding_metadata.syntax();
    let names = inventory
        .declarations()
        .iter()
        .map(|declaration| {
            (
                declaration.kind(),
                declaration.name(),
                slice(source, declaration.location()),
            )
        })
        .collect::<Vec<_>>();
    use crate::config_loader::DeclarationKind::*;
    assert_eq!(
        names,
        [
            (ProviderKind, "mock", "mock"),
            (Argument, "input", "input"),
            (ModuleAttribute, "output", "output"),
            (Export, "same", "same"),
            (Export, "same", "same"),
            (Binding, "state", "state"),
            (UserFunction, "identity", "identity")
        ]
    );
    let DeclarationAnnotation::Authored(annotation) = inventory.declarations()[3].annotation()
    else {
        panic!("typed export annotation")
    };
    assert_eq!(slice(source, annotation), "String");
    assert!(matches!(
        inventory.declarations()[4].annotation(),
        DeclarationAnnotation::Unannotated
    ));
    let upstream = &inventory.upstream_sources()[0];
    assert_eq!(upstream.detail().as_str(), "state");
    assert_eq!(slice(source, upstream.location()), "'../upstream'");
}

#[test]
fn invalid_upstream_source_remains_a_file_scoped_parse_error() {
    let result = parse_file_scoped(
        "let state = upstream_state {}",
        Path::new("sibling.crn"),
        &context(),
    );
    let error = result.unwrap_err();
    assert_eq!(error.file().path(), Path::new("sibling.crn"));
    assert!(error.detail().to_string().contains("source"));
}

#[test]
fn exports_consuming_sibling_let_keep_the_reference_owners_file() {
    let directory = tempfile::tempdir().unwrap();
    let main = "let target = mock.test.Resource {}\nlet value = target.items[0].name\n";
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
    std::fs::write(
        directory.path().join("exports.crn"),
        "exports { out = value }\n",
    )
    .unwrap();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context(),
    )
    .unwrap();
    let export = parsed.parsed().export_params[0].value.as_ref().unwrap();
    let mut count = 0;
    export.value().unwrap().visit_resource_refs(&mut |path| {
        let source = path.reference_locations().unwrap();
        assert_eq!(
            source.occurrence().file().path(),
            directory.path().join("main.crn")
        );
        assert_eq!(slice(main, source.occurrence()), "target.items[0].name");
        count += 1;
    });
    assert_eq!(count, 1);
}

#[test]
fn parenthesized_handle_postfix_keeps_the_whole_occurrence()
-> Result<(), crate::parser::AnalysisTreeError> {
    let source = "let target = mock.test.Resource {}\nmock.test.Resource { value = (target).items[0].name }\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed)?;
    let holder = tree
        .resources()
        .find(|resource| resource.binding().is_none())
        .unwrap();
    assert_eq!(
        slice(source, holder.references()[0].location()),
        "(target).items[0].name"
    );
    Ok(())
}

#[test]
fn function_body_access_occurrence_survives_argument_substitution()
-> Result<(), crate::parser::AnalysisTreeError> {
    let functions =
        "// original function source\nfn name_of(r: mock.test.Resource): String { r.status }\n";
    let main =
        "let target = mock.test.Resource {}\nmock.test.Resource { value = name_of(target) }\n";
    let files = crate::config_loader::parse_directory_files(
        &[
            ("/fixture/functions.crn".into(), functions.into()),
            ("/fixture/main.crn".into(), main.into()),
        ],
        &context(),
    )
    .unwrap();
    let function_file = files
        .iter()
        .find(|(path, _)| path.ends_with("functions.crn"))
        .unwrap()
        .1
        .parsed();
    let function = &function_file.user_functions["name_of"];
    let substitutions = std::collections::HashMap::from([(
        "r".into(),
        crate::resource::Value::Deferred(crate::resource::DeferredValue::BindingRef {
            binding: RelativeBindingName::new("target"),
        })
        .into(),
    )]);
    let substituted = super::functions::substitute_fn_params(
        &function.body.value,
        &substitutions,
        function.body.source_line,
    )
    .unwrap();
    let mut parsed = files
        .into_iter()
        .find(|(path, _)| path.ends_with("main.crn"))
        .unwrap()
        .1
        .into_inner();
    parsed.resources[1]
        .attributes
        .insert("value".into(), substituted);
    let tree = AnalysisTree::try_from_file(&parsed)?;
    let holder = tree
        .resources()
        .find(|resource| resource.binding().is_none())
        .unwrap();
    let reference = &holder.references()[0];
    assert_eq!(
        reference.location().file().path(),
        Path::new("/fixture/functions.crn")
    );
    assert_eq!(slice(functions, reference.location()), "r.status");
    assert_eq!(slice(functions, reference.prefix_location()), "r.status");
    Ok(())
}

#[test]
fn single_nested_block_key_paths_match_parser_sink_paths()
-> Result<(), crate::parser::AnalysisTreeError> {
    let source = "mock.test.Resource { rule { value = 'one' } }";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed)?;
    let resource = tree.resources().next().unwrap();
    assert_eq!(
        resource.attribute_keys()[1].path(),
        [
            AttributePathSegment::Attribute("rule".into()),
            AttributePathSegment::ListIndex(0),
            AttributePathSegment::StructField("value".into()),
        ]
    );
    Ok(())
}

#[test]
fn analysis_boundary_rejects_missing_export_and_type_inventory() {
    for source in ["exports { result = 1 }", "arguments { value: String }"] {
        let mut parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
        parsed.parser_binding_metadata = Default::default();
        assert!(
            matches!(
                AnalysisTree::try_from_file(&parsed)
                    .err()
                    .as_ref()
                    .map(AnalysisTreeError::kind),
                Some(AnalysisTreeErrorKind::MissingSyntaxProvenance { .. })
            ),
            "unprovenanced analysis input: {source}"
        );
    }
}

#[test]
fn analysis_boundary_rejects_missing_merged_renderer_inventory() {
    for source in [
        "mock.test.Resource { value = 'x' }",
        "for item in ['x'] { mock.test.Resource { value = item } }",
        "provider mock {}",
        "moved { from = mock.test.Resource 'old' to = mock.test.Resource '_for0[0]' }",
    ] {
        let mut parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
        parsed.parser_binding_metadata = Default::default();
        assert!(
            matches!(
                AnalysisTree::try_from_file(&parsed)
                    .err()
                    .as_ref()
                    .map(AnalysisTreeError::kind),
                Some(AnalysisTreeErrorKind::MissingSyntaxProvenance { .. })
            ),
            "unprovenanced merged renderer input: {source}"
        );
    }
}

#[test]
fn loop_inventory_requires_exact_file_identity() {
    let source = "for x in ['a'] { mock.test.Resource { name = x } }\n";
    let files = crate::config_loader::parse_directory_files(
        &[
            ("main.crn".into(), source.into()),
            ("module/main.crn".into(), source.into()),
        ],
        &context(),
    )
    .unwrap();
    let mut files = files.into_iter();
    let mut root = files.next().unwrap().1.into_inner();
    let module = files.next().unwrap().1.into_inner();
    assert!(AnalysisTree::try_from_file(&root).is_ok());
    assert!(AnalysisTree::try_from_file(&module).is_ok());
    // Simulate expansion losing the root inventory while retaining an
    // identically positioned loop from module/main.crn.
    root.parser_binding_metadata = module.parser_binding_metadata;
    assert!(
        AnalysisTree::try_from_file(&root).is_err(),
        "module/main.crn must not supply provenance for a loop in main.crn"
    );
}

#[test]
fn provider_analysis_returns_missing_provenance() {
    let mut parsed = provider_directory();
    parsed.parser_binding_metadata = Default::default();
    assert!(matches!(
        resolve_provider_unresolved_attributes_for_analysis(&mut parsed, &context()),
        Err(AnalysisResolutionError::Invariant(_))
    ));
}

#[test]
fn expanded_provider_analysis_returns_missing_provenance() {
    let mut parsed = provider_directory().resolve_root_for_test();
    parsed.parser_binding_metadata = Default::default();
    assert!(matches!(
        resolve_expanded_provider_unresolved_attributes_for_analysis(&mut parsed, &context()),
        Err(AnalysisResolutionError::Invariant(_))
    ));
}

#[test]
fn provider_finalization_returns_missing_provenance() {
    let mut parsed = provider_directory();
    parsed.parser_binding_metadata = Default::default();
    assert!(matches!(
        finalize_provider_configs_for_analysis(&mut parsed),
        Err(AnalysisResolutionError::Invariant(_))
    ));
}

fn provider_directory() -> ParsedFile {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.crn"), "let unrelated = 'main'\n").unwrap();
    std::fs::write(
        root.path().join("sibling.crn"),
        "provider mock { default_tags = {} }\n",
    )
    .unwrap();
    crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(root.path()).unwrap(),
        &context(),
    )
    .unwrap()
    .into_parsed()
}

#[test]
fn filtered_dependencies_keep_later_operand_locations() {
    let source = "mock.test.Resource { directives { depends_on = [first, first, second] } }\n";
    let files = crate::config_loader::parse_directory_files(
        &[
            ("main.crn".into(), "let unrelated = 'main'".into()),
            ("sibling.crn".into(), source.into()),
        ],
        &context(),
    )
    .unwrap();
    let parsed = files.into_iter().nth(1).unwrap().1.into_inner();
    let dependencies = &parsed.resources[0].dependencies.depends_on;
    let filtered: UnexpandedDependencies = dependencies
        .try_filter_map(|index, operand| {
            Ok::<_, std::convert::Infallible>((index != 0).then(|| operand.detail().clone()))
        })
        .unwrap();
    assert_eq!(filtered.diagnostic_references(), ["first", "second"]);
    for (operand, start) in filtered.located_operands().iter().zip([
        source.match_indices("first").nth(1).unwrap().0,
        source.find("second").unwrap(),
    ]) {
        assert_eq!(operand.location().file().path(), Path::new("sibling.crn"));
        assert_eq!(operand.location().span().start_byte(), start);
        assert_eq!(
            slice(source, operand.location()),
            operand.detail().diagnostic_reference()
        );
    }
    let remapped =
        filtered.map_bindings(&mut |binding| crate::resource::RootScope::new().resolve(binding));
    assert_eq!(
        remapped.located_operands()[0].location(),
        dependencies.located_operands()[1].location()
    );
    assert_eq!(
        remapped.located_operands()[1].location(),
        dependencies.located_operands()[2].location()
    );
}
