use super::*;
use std::path::Path;

fn context() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}
fn slice<'a>(source: &'a str, location: &DeclarationLocation) -> &'a str {
    &source[location.span().start_byte()..location.span().end_byte()]
}

#[test]
fn nested_keys_and_repeated_blocks_keep_occurrence_paths() {
    let source = "mock.test.Resource {\n  settings = { inner = [{ label = 'a' }, { label = 'b' }] }\n  rule { name = 'first' }\n  rule { name = 'second' }\n}\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
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
}

#[test]
fn reference_occurrences_and_prefixes_preserve_interpolation_postfix_and_heredocs() {
    let source = "let target = mock.test.Resource {}\nlet holder = mock.test.Resource {\n  note = <<-EOT\n    😀 unicode\n  EOT\n  values = [target.items[0].name, (target.items)[1].name, \"${target.items[2].name}\", target.items.123]\n}\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
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
fn parenthesized_handle_postfix_keeps_the_whole_occurrence() {
    let source = "let target = mock.test.Resource {}\nmock.test.Resource { value = (target).items[0].name }\n";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    let holder = tree
        .resources()
        .find(|resource| resource.binding().is_none())
        .unwrap();
    assert_eq!(
        slice(source, holder.references()[0].location()),
        "(target).items[0].name"
    );
}

#[test]
fn function_body_access_occurrence_survives_argument_substitution() {
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
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
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
}

#[test]
fn single_nested_block_key_paths_match_parser_sink_paths() {
    let source = "mock.test.Resource { rule { value = 'one' } }";
    let parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    let resource = tree.resources().next().unwrap();
    assert_eq!(
        resource.attribute_keys()[1].path(),
        [
            AttributePathSegment::Attribute("rule".into()),
            AttributePathSegment::ListIndex(0),
            AttributePathSegment::StructField("value".into()),
        ]
    );
}

#[test]
fn analysis_boundary_rejects_missing_export_and_type_inventory() {
    for source in ["exports { result = 1 }", "arguments { value: String }"] {
        let mut parsed = parse_file(source, Path::new("sibling.crn"), &context()).unwrap();
        parsed.parser_binding_metadata = Default::default();
        assert!(
            matches!(
                AnalysisTree::try_from_file(&parsed),
                Err(AnalysisTreeError::MissingSyntaxProvenance { .. })
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
                AnalysisTree::try_from_file(&parsed),
                Err(AnalysisTreeError::MissingSyntaxProvenance { .. })
            ),
            "unprovenanced merged renderer input: {source}"
        );
    }
}
