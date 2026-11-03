use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{parse_directory, parse_directory_files};
use carina_core::module_resolver::resolve_modules_providerless;
use carina_core::parser::{
    AnalysisItemKind, AnalysisTree, AnalysisTreeErrorKind, DeclarationLocation, ParsedFile,
    ProviderContext,
};
use carina_core::paths::ResolvedPath;
use carina_core::resource::Resource;
use carina_core::validation::deferred_types::{
    ProviderTypeCheckDiagnostic, ProviderTypeCheckSink, TypeCheckSession,
};

fn context() -> ProviderContext {
    ProviderContext::new(carina_core::EnvironmentSnapshot::empty())
}

fn assert_location(location: &DeclarationLocation, file: &Path, source: &str, declaration: &str) {
    assert_eq!(location.file().path(), file);
    let start = source.find(declaration).expect("declaration in fixture");
    let span = location.span();
    assert_eq!(span.start_byte(), start, "{declaration}");
    assert_eq!(span.end_byte(), start + declaration.len(), "{declaration}");
    assert_eq!(
        span.start_line(),
        source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count()
            + 1,
    );
    assert_eq!(&source[span.start_byte()..span.end_byte()], declaration);
}

#[test]
fn directory_declaration_provenance_covers_named_anonymous_static_and_deferred_bodies()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let main = "arguments { enabled: Bool names: list(String) }\nlet names_now = ['a', 'b']\nlet remote = upstream_state { source = '../remote' }\n";
    let sibling = "\
let named = mock.test.Resource { name = 'named' }
mock.test.Resource { name = 'anonymous' }
let lookup = read mock.test.Lookup { name = 'lookup' }
let ready = wait named { until = named.status == 'ready' }
let static_items = for n in names_now { mock.test.Resource { name = n } }
for n in names_now { mock.test.Resource { anonymous_name = n } }
let static_reads = for n in names_now { read mock.test.Lookup { name = n } }
let deferred_items = for n in names { mock.test.Resource { deferred_name = n } }
let upstream_items = for n in remote.items { mock.test.Resource { upstream_name = n } }
let deferred_reads = for n in names { read mock.test.Lookup { deferred_name = n } }
let selected = if enabled { mock.test.Resource { name = 'then' } } else { mock.test.Resource { name = 'else' } }
let selected_read = if enabled { read mock.test.Lookup { name = 'then read' } } else { read mock.test.Lookup { name = 'else read' } }
";
    let main_path = PathBuf::from("/fixture/main.crn");
    let sibling_path = PathBuf::from("/fixture/sibling.crn");
    let files = parse_directory_files(
        &[
            (main_path, main.into()),
            (sibling_path.clone(), sibling.into()),
        ],
        &context(),
    )
    .expect("multi-file directory parse");
    let parsed = files
        .into_iter()
        .find(|(file, _)| file == &sibling_path)
        .unwrap()
        .1
        .into_inner();
    let analysis = AnalysisTree::try_from_file(&parsed)?;
    let declarations = [
        ("mock.test.Resource { name = 'named' }", 1),
        ("mock.test.Resource { name = 'anonymous' }", 1),
        ("read mock.test.Lookup { name = 'lookup' }", 1),
        ("mock.test.Resource { name = n }", 2),
        ("mock.test.Resource { anonymous_name = n }", 2),
        ("read mock.test.Lookup { name = n }", 2),
        ("mock.test.Resource { deferred_name = n }", 1),
        ("mock.test.Resource { upstream_name = n }", 1),
        ("read mock.test.Lookup { deferred_name = n }", 1),
        ("mock.test.Resource { name = 'then' }", 1),
        ("mock.test.Resource { name = 'else' }", 1),
        ("read mock.test.Lookup { name = 'then read' }", 1),
        ("read mock.test.Lookup { name = 'else read' }", 1),
    ];
    assert_eq!(
        analysis.resources().count(),
        declarations.iter().map(|(_, count)| count).sum::<usize>()
    );
    for (declaration, count) in declarations {
        let start = sibling.find(declaration).unwrap();
        let items: Vec<_> = analysis
            .resources()
            .filter(|item| item.location().span().start_byte() == start)
            .collect();
        assert_eq!(items.len(), count, "{declaration}");
        for item in items {
            assert_location(item.location(), &sibling_path, sibling, declaration);
        }
    }
    let wait = analysis.waits().next().expect("located wait");
    assert_location(
        wait.location(),
        &sibling_path,
        sibling,
        "wait named { until = named.status == 'ready' }",
    );
    Ok(())
}

struct NoTypeErrors;

impl ProviderTypeCheckSink for NoTypeErrors {
    fn record(&mut self, diagnostic: ProviderTypeCheckDiagnostic) {
        panic!("unexpected fixture diagnostic: {diagnostic:?}");
    }
}

fn expand_directory(root: &Path) -> carina_core::parser::ExpandedParsedFile {
    let path = ResolvedPath::new(root).unwrap();
    let config = context();
    TypeCheckSession::run(&config, |session| {
        let parsed = parse_directory(&path, session.catalog()).expect("directory parse");
        resolve_modules_providerless(parsed, &path, session).expect("module expansion")
    })
    .publish(&mut NoTypeErrors)
    .into_expanded(&carina_core::schema::SchemaRegistry::new(), &config)
    .expect("placeholder expansion")
}

#[test]
fn review_round1_module_private_bindings_do_not_enter_root_iterable_scope()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("module")).unwrap();
    let sibling = "for x in vpc.ids { mock.test.Resource { name = x } }\n";
    for (name, source) in [
        (
            "main.crn",
            "let child = use { source = './module' }\nlet instance = child {}\n",
        ),
        ("sibling.crn", sibling),
        (
            "module/arguments.crn",
            "arguments { unused: Bool = true }\n",
        ),
        (
            "module/main.crn",
            "let vpc = mock.test.Resource { name = 'private' }\n",
        ),
    ] {
        std::fs::write(root.path().join(name), source).unwrap();
    }
    let path = ResolvedPath::new(root.path()).unwrap();
    let expanded = TypeCheckSession::run(&context(), |session| {
        let parsed = parse_directory(&path, session.catalog()).unwrap();
        resolve_modules_providerless(parsed, &path, session).unwrap()
    })
    .publish(&mut NoTypeErrors);
    let tree = AnalysisTree::try_from_file(&expanded)?;
    let findings = carina_core::validation::validate_for_iterable_bindings(&tree);
    assert_eq!(
        findings.len(),
        1,
        "module-private vpc leaked into root scope"
    );
    assert_location(
        findings[0].location(),
        &root.path().join("sibling.crn"),
        sibling,
        "vpc.ids",
    );
    assert!(
        findings[0]
            .to_string()
            .contains("Undefined identifier `vpc`")
    );
    Ok(())
}

#[test]
fn review_round1_root_and_module_loops_at_the_same_span_keep_distinct_files()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("module")).unwrap();
    let loop_source = "for x in ['one'] { mock.test.Resource { name = x } }\n";
    std::fs::write(root.path().join("main.crn"), loop_source).unwrap();
    std::fs::write(
        root.path().join("sibling.crn"),
        "let child = use { source = './module' }\nlet instance = child {}\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("module/main.crn"),
        format!("{loop_source}arguments {{ unused: Bool = true }}\n"),
    )
    .unwrap();
    let expanded = expand_directory(root.path());
    let tree = AnalysisTree::try_from_file(&expanded)?;
    let loops: Vec<_> = tree
        .resources()
        .filter(|item| item.kind() != AnalysisItemKind::Composition)
        .collect();
    assert_eq!(loops.len(), 2);
    assert_eq!(loops[0].location().span(), loops[1].location().span());
    let files: std::collections::HashSet<_> = loops
        .iter()
        .map(|item| item.location().file().path().to_path_buf())
        .collect();
    assert_eq!(
        files,
        [
            root.path().join("main.crn"),
            root.path().join("module/main.crn")
        ]
        .into_iter()
        .collect()
    );
    Ok(())
}

#[test]
fn directory_declaration_provenance_survives_module_expansion_and_renamed_ids()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let main = "let child = use { source = './child' }\nlet remote = upstream_state { source = '../remote' }\n";
    let sibling = "let first = child { enabled = true names = ['a', 'b'] later_names = remote.items }\nlet second = child { enabled = false names = ['c'] later_names = remote.items }\n";
    let child_main = "arguments { enabled: Bool names: list(String) later_names: list(String) }\n";
    let child_sibling = "\
let named = mock.test.Resource { name = 'child' }
mock.test.Resource { name = 'anonymous child' }
let lookup = read mock.test.Lookup { name = 'child lookup' }
let ready = wait named { until = named.status == 'ready' }
let generated = for n in names { mock.test.Resource { name = n } }
let generated_reads = for n in ['a', 'b'] { read mock.test.Lookup { name = n } }
let later = for n in later_names { mock.test.Resource { deferred_name = n } }
let selected = if enabled { mock.test.Resource { name = 'then' } } else { mock.test.Resource { name = 'else' } }
let selected_read = if enabled { read mock.test.Lookup { name = 'then read' } } else { read mock.test.Lookup { name = 'else read' } }
";
    for (file, source) in [
        ("main.crn", main),
        ("sibling.crn", sibling),
        ("child/main.crn", child_main),
        ("child/sibling.crn", child_sibling),
    ] {
        std::fs::write(root.path().join(file), source).unwrap();
    }
    let config = context();
    let expanded = expand_directory(root.path());
    let analysis = AnalysisTree::try_from_file(&expanded)?;
    assert_eq!(analysis.resources().count(), 20);
    let child_path = root.path().join("child/sibling.crn");
    let caller_path = root.path().join("sibling.crn");
    for item in analysis.resources() {
        if item.kind() == AnalysisItemKind::Composition {
            let declaration = match item.binding() {
                Some("first") => {
                    "child { enabled = true names = ['a', 'b'] later_names = remote.items }"
                }
                Some("second") => {
                    "child { enabled = false names = ['c'] later_names = remote.items }"
                }
                other => panic!("unexpected composition {other:?}"),
            };
            assert_location(item.location(), &caller_path, sibling, declaration);
        } else {
            let declaration = match item.binding() {
                None => "mock.test.Resource { name = 'anonymous child' }",
                Some(binding) => {
                    assert!(
                        binding.starts_with("first.") || binding.starts_with("second."),
                        "{binding}"
                    );
                    assert_eq!(item.id().identity_str(), Some(binding));
                    let local = binding.split_once('.').unwrap().1;
                    match local {
                        "named" => "mock.test.Resource { name = 'child' }",
                        "lookup" => "read mock.test.Lookup { name = 'child lookup' }",
                        "selected" if binding.starts_with("first.") => {
                            "mock.test.Resource { name = 'then' }"
                        }
                        "selected" => "mock.test.Resource { name = 'else' }",
                        "selected_read" if binding.starts_with("first.") => {
                            "read mock.test.Lookup { name = 'then read' }"
                        }
                        "selected_read" => "read mock.test.Lookup { name = 'else read' }",
                        "generated[?]" => "mock.test.Resource { name = n }",
                        "later[?]" => "mock.test.Resource { deferred_name = n }",
                        "generated_reads[0]" | "generated_reads[1]" => {
                            "read mock.test.Lookup { name = n }"
                        }
                        other => panic!("unexpected expanded binding {other}"),
                    }
                }
            };
            assert_location(item.location(), &child_path, child_sibling, declaration);
        }
    }
    assert_eq!(analysis.waits().count(), 2);
    for wait in analysis.waits() {
        assert_location(
            wait.location(),
            &child_path,
            child_sibling,
            "wait named { until = named.status == 'ready' }",
        );
    }
    for (binding, declaration) in [
        ("first.selected", "mock.test.Resource { name = 'then' }"),
        ("second.selected", "mock.test.Resource { name = 'else' }"),
        (
            "first.selected_read",
            "read mock.test.Lookup { name = 'then read' }",
        ),
        (
            "second.selected_read",
            "read mock.test.Lookup { name = 'else read' }",
        ),
        ("first.generated[?]", "mock.test.Resource { name = n }"),
        ("second.generated[?]", "mock.test.Resource { name = n }"),
    ] {
        let item = analysis
            .resources()
            .find(|item| item.binding() == Some(binding))
            .unwrap_or_else(|| panic!("missing {binding}"));
        assert_location(item.location(), &child_path, child_sibling, declaration);
    }

    // Instantiating children rewrites ids again and changes the value phase.
    // Every child must retain its body's location, including runtime iterables.
    let concrete =
        carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::List(vec![
            carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
                "materialized".into(),
            )),
        ]));
    for deferred in expanded.deferred_for_expressions().iter() {
        let children = TypeCheckSession::run(&config, |session| {
            carina_core::parser::expand_deferred_children(
                deferred,
                &concrete,
                carina_core::resource::AccessMissPolicy::Error,
                session,
            )
            .unwrap()
        })
        .publish(&mut NoTypeErrors);
        let mut child_tree = carina_core::parser::ExpandedParsedFile::default();
        child_tree.resources = children;
        let analysis = AnalysisTree::try_from_file(&child_tree)?;
        let item = analysis.resources().next().unwrap();
        assert_ne!(item.id(), deferred.template_resource_id());
        let declaration = if item.binding().unwrap().contains(".generated[") {
            "mock.test.Resource { name = n }"
        } else {
            "mock.test.Resource { deferred_name = n }"
        };
        assert_location(item.location(), &child_path, child_sibling, declaration);
    }

    let mut unlocated = expanded.clone();
    unlocated.compositions[0].declaration = Default::default();
    let error = match AnalysisTree::try_from_file(&unlocated) {
        Ok(_) => panic!("unlocated composition entered analysis"),
        Err(error) => error,
    };
    assert!(matches!(
        error.kind(),
        AnalysisTreeErrorKind::MissingResourceProvenance {
            kind: AnalysisItemKind::Composition,
            ..
        }
    ));
    assert_eq!(unlocated.compositions, expanded.compositions);
    Ok(())
}

#[test]
fn directory_declaration_provenance_preserves_nested_composition_call_sites()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("outer/inner")).unwrap();
    let caller = "let inst = outer {}\n";
    let outer = "let nested = inner {}\n";
    let inner =
        "arguments { unused: Bool = true }\nlet named = mock.test.Resource { name = 'nested' }\n";
    for (file, source) in [
        ("main.crn", "let outer = use { source = './outer' }\n"),
        ("sibling.crn", caller),
        (
            "outer/main.crn",
            "arguments { unused: Bool = true }\nlet inner = use { source = './inner' }\n",
        ),
        ("outer/sibling.crn", outer),
        ("outer/inner/main.crn", inner),
    ] {
        std::fs::write(root.path().join(file), source).unwrap();
    }
    let expanded = expand_directory(root.path());
    let analysis = AnalysisTree::try_from_file(&expanded)?;
    assert_eq!(analysis.resources().count(), 3);
    for item in analysis.resources() {
        match item.binding() {
            Some("inst") => assert_location(
                item.location(),
                &root.path().join("sibling.crn"),
                caller,
                "outer {}",
            ),
            Some("inst.nested") => assert_location(
                item.location(),
                &root.path().join("outer/sibling.crn"),
                outer,
                "inner {}",
            ),
            Some("inst.nested.named") => assert_location(
                item.location(),
                &root.path().join("outer/inner/main.crn"),
                inner,
                "mock.test.Resource { name = 'nested' }",
            ),
            binding => panic!("unexpected binding {binding:?}"),
        }
    }
    Ok(())
}

#[test]
fn directory_declaration_provenance_uses_original_unicode_crlf_and_heredoc_spans()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let source = "// 東京\r\nlet named = mock.test.Resource {\r\n  doc = <<EOT\r\n日本語\r\nsecond line\r\nEOT\r\n}\r\nlet ready = wait named { until = named.status == 'ready' }\r\nlet lookup = read mock.test.Lookup { name = 'after' }\r\n";
    let sibling_path = PathBuf::from("/fixture/sibling.crn");
    let files = parse_directory_files(
        &[
            ("/fixture/main.crn".into(), "let unused = true\n".into()),
            (sibling_path.clone(), source.into()),
        ],
        &context(),
    )
    .unwrap();
    let parsed = files
        .into_iter()
        .find(|(file, _)| file == &sibling_path)
        .unwrap()
        .1
        .into_inner();
    let analysis = AnalysisTree::try_from_file(&parsed)?;
    for item in analysis.resources() {
        let declaration = match item.binding() {
            Some("named") => {
                "mock.test.Resource {\r\n  doc = <<EOT\r\n日本語\r\nsecond line\r\nEOT\r\n}"
            }
            Some("lookup") => "read mock.test.Lookup { name = 'after' }",
            other => panic!("unexpected binding {other:?}"),
        };
        assert_location(item.location(), &sibling_path, source, declaration);
        assert_eq!(
            item.location().span().start_column(),
            13 + usize::from(item.binding() == Some("lookup"))
        );
    }
    assert_location(
        analysis.waits().next().unwrap().location(),
        &sibling_path,
        source,
        "wait named { until = named.status == 'ready' }",
    );
    Ok(())
}

#[test]
fn declaration_provenance_is_semantically_neutral_and_not_serialized()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let source = "let named = mock.test.Resource { name = 'same' }\nlet lookup = read mock.test.Lookup { name = 'same' }\n";
    let files = parse_directory_files(
        &[
            ("/fixture/main.crn".into(), source.into()),
            ("/fixture/sibling.crn".into(), format!("\n\n{source}")),
        ],
        &context(),
    )
    .unwrap();
    let mut parsed: Vec<_> = files
        .into_iter()
        .map(|(_, file)| file.into_inner())
        .collect();
    assert_eq!(parsed[0].resources, parsed[1].resources);
    assert_eq!(parsed[0].data_sources, parsed[1].data_sources);
    let mut hashes = Vec::new();
    for file in &parsed {
        let mut hasher = DefaultHasher::new();
        file.resources[0].declaration.hash(&mut hasher);
        hashes.push(hasher.finish());
        let mut resource: Resource = Resource::new("test.Resource", "named");
        resource.declaration = file.resources[0].declaration.clone();
        let mut data_source: carina_core::resource::DataSource =
            carina_core::resource::DataSource::new("test.Lookup", "lookup");
        data_source.declaration = file.data_sources[0].declaration.clone();
        let resource_json = serde_json::to_value(resource.into_value_snapshot()).unwrap();
        let data_source_json = serde_json::to_value(data_source.into_value_snapshot()).unwrap();
        assert!(resource_json.get("declaration").is_none());
        assert!(data_source_json.get("declaration").is_none());
    }
    assert_eq!(hashes[0], hashes[1]);
    let sibling = parsed.pop().unwrap();
    parsed[0].resources.extend(sibling.resources);
    let analysis = AnalysisTree::try_from_file(&parsed[0])?;
    let locations: Vec<_> = analysis
        .resources()
        .filter(|item| item.kind() == AnalysisItemKind::Resource)
        .map(|item| item.location())
        .collect();
    assert_eq!(locations.len(), 2);
    assert_ne!(
        locations[0], locations[1],
        "equal infrastructure ids retain both declarations"
    );
    Ok(())
}

#[test]
fn analysis_tree_rejects_missing_declaration_provenance() {
    let mut parsed = ParsedFile::default();
    parsed
        .resources
        .push(Resource::new("test.Resource", "synthetic"));
    let error = match AnalysisTree::try_from_file(&parsed) {
        Ok(_) => panic!("programmatic resource must not enter analysis"),
        Err(error) => error,
    };
    assert!(matches!(
        error.kind(),
        AnalysisTreeErrorKind::MissingResourceProvenance {
            kind: AnalysisItemKind::Resource,
            ..
        }
    ));
}

#[test]
fn element_provenance_survives_module_expansion_and_deferred_phase_changes()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    let main = "let child = use { source = './child' }\nlet remote = upstream_state { source = '../remote' }\nlet instance = child { names = remote.items }\n";
    let child = "arguments { names: list(String) }\nlet target = mock.test.Resource {}\nlet consumer = mock.test.Resource {\n  value = target.status\n  directives { depends_on = [target, target] }\n}\nlet ready = wait target { until = target.status == 'ready' depends_on = [target, target] }\nlet items = for name in names { mock.test.Resource { name = name } }\nattributes { output: String = target.status }\n";
    std::fs::write(root.path().join("main.crn"), main).unwrap();
    let child_path = root.path().join("child/main.crn");
    std::fs::write(&child_path, child).unwrap();
    let expanded = expand_directory(root.path());
    let tree = AnalysisTree::try_from_file(&expanded)?;
    let consumer = tree
        .resources()
        .find(|resource| {
            resource
                .binding()
                .is_some_and(|binding| binding.ends_with(".consumer"))
        })
        .unwrap();
    assert_eq!(consumer.dependencies().len(), 2);
    let first_operand = child.find("[target, target]").unwrap() + 1;
    for (dependency, start) in consumer
        .dependencies()
        .iter()
        .zip([first_operand, first_operand + "target, ".len()])
    {
        assert_eq!(dependency.location().file().path(), child_path);
        assert_eq!(dependency.location().span().start_byte(), start);
        assert_eq!(
            dependency.location().span().end_byte(),
            start + "target".len()
        );
    }
    assert_ne!(
        consumer.dependencies()[0].location(),
        consumer.dependencies()[1].location()
    );
    let reference = &consumer.references()[0];
    assert_location(reference.location(), &child_path, child, "target.status");
    assert!(reference.path().binding().ends_with(".target"));
    let wait = tree.waits().next().unwrap();
    assert_eq!(wait.dependencies().len(), 2);
    assert_eq!(wait.target_location().file().path(), child_path);
    assert_eq!(
        &child[wait.lhs_location().span().start_byte()..wait.lhs_location().span().end_byte()],
        "target.status"
    );
    let deferred = tree
        .resources()
        .find(|resource| resource.kind() == AnalysisItemKind::DeferredResource)
        .unwrap();
    let iterable = deferred.deferred_iterable().unwrap();
    assert_eq!(iterable.location().file().path(), child_path);
    assert_eq!(
        &child[iterable.location().span().start_byte()..iterable.location().span().end_byte()],
        "names"
    );
    assert!(
        tree.for_iterables()
            .iter()
            .any(|iterable| iterable.location().file().path() == child_path)
    );
    assert!(
        tree.syntax()
            .declarations()
            .iter()
            .any(|declaration| declaration.name() == "output"
                && declaration.location().file().path() == child_path)
    );
    let composition = tree
        .resources()
        .find(|resource| resource.kind() == AnalysisItemKind::Composition)
        .unwrap();
    let output = composition
        .attribute_keys()
        .iter()
        .find(|attribute| {
            attribute.path()
                == [carina_core::parser::AttributePathSegment::Attribute(
                    "output".into(),
                )]
        })
        .unwrap();
    assert_location(output.location(), &child_path, child, "output");
    Ok(())
}

#[test]
fn module_argument_forwarding_preserves_reference_occurrences()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let root = tempfile::tempdir().unwrap();
    for directory in ["outer", "inner"] {
        std::fs::create_dir(root.path().join(directory)).unwrap();
    }
    let caller = "let outer = use { source = './outer' }\nlet target = mock.test.Resource {}\nlet instance = outer { supplied = target }\n";
    let outer = "arguments { supplied: mock.test.Resource }\nlet inner = use { source = '../inner' }\nlet nested = inner { supplied = supplied }\n";
    let inner = "arguments { supplied: mock.test.Resource }\n";
    let outputs = "attributes {\n  direct = supplied.status\n  repeated = [supplied.status, supplied.status]\n  postfix = (supplied.status)[0]\n  interpolated = \"${supplied.status}\"\n}\n";
    for (name, source) in [
        ("main.crn", caller),
        ("outer/main.crn", outer),
        ("inner/main.crn", inner),
        ("inner/sibling.crn", outputs),
    ] {
        std::fs::write(root.path().join(name), source).unwrap();
    }
    let mut config = context();
    config
        .resource_types
        .insert(("mock".into(), "test.Resource".into()));
    let path = ResolvedPath::new(root.path()).unwrap();
    let expanded = TypeCheckSession::run(&config, |session| {
        let parsed = parse_directory(&path, session.catalog()).unwrap();
        resolve_modules_providerless(parsed, &path, session).unwrap()
    })
    .publish(&mut NoTypeErrors)
    .into_expanded(&carina_core::schema::SchemaRegistry::new(), &config)
    .unwrap();
    let tree = AnalysisTree::try_from_file(&expanded)?;
    let composition = tree
        .resources()
        .find(|item| item.binding() == Some("instance.nested"))
        .unwrap();
    let references = composition.references();
    assert_eq!(references.len(), 5);
    let output_path = root.path().join("outer/../inner/sibling.crn");
    for (reference, expression) in references.iter().zip([
        "supplied.status",
        "supplied.status",
        "supplied.status",
        "(supplied.status)[0]",
        "supplied.status",
    ]) {
        let location = reference.location();
        assert_eq!(reference.path().binding(), "target");
        assert_eq!(reference.path().attribute(), "status");
        assert_eq!(location.file().path(), output_path);
        assert_eq!(
            &outputs[location.span().start_byte()..location.span().end_byte()],
            expression,
        );
        let prefix = reference.prefix_location();
        assert_eq!(prefix.file().path(), output_path);
        assert_eq!(
            &outputs[prefix.span().start_byte()..prefix.span().end_byte()],
            "supplied.status",
        );
    }
    assert_ne!(references[1].location(), references[2].location());
    Ok(())
}
