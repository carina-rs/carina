//! Directory acceptance tests for parser-owned diagnostic locations (#4220).
use super::*;

struct Directory {
    root: tempfile::TempDir,
}

impl Directory {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        for (name, source) in files {
            let path = root.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        Self { root }
    }

    fn assert_ranges(
        &self,
        engine: &DiagnosticEngine,
        name: &str,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        let source = std::fs::read_to_string(self.root.path().join(name)).unwrap();
        self.assert_buffer_ranges(engine, name, &source, message, tokens);
    }

    fn assert_buffer_ranges(
        &self,
        engine: &DiagnosticEngine,
        name: &str,
        source: &str,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        let path = self.root.path().join(name);
        let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
        let mut actual: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains(message))
            .map(|diagnostic| diagnostic.range)
            .collect();
        let mut expected: Vec<_> = tokens
            .iter()
            .map(|(token, occurrence)| {
                let start = source.match_indices(token).nth(*occurrence).unwrap().0;
                let position = |byte| {
                    let prefix = &source[..byte];
                    Position::new(
                        prefix.bytes().filter(|byte| *byte == b'\n').count() as u32,
                        prefix.rsplit('\n').next().unwrap().encode_utf16().count() as u32,
                    )
                };
                Range::new(position(start), position(start + token.len()))
            })
            .collect();
        let order = |range: &Range| {
            (
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character,
            )
        };
        actual.sort_by_key(order);
        expected.sort_by_key(order);
        assert_eq!(actual, expected, "{name}: {diagnostics:#?}");
    }

    fn assert_sibling_owner(
        &self,
        engine: &DiagnosticEngine,
        message: &str,
        tokens: &[(&str, usize)],
    ) {
        self.assert_ranges(engine, "main.crn", message, &[]);
        self.assert_ranges(engine, "sibling.crn", message, tokens);
    }
}

const MAIN: &str = "provider aws {}\nlet cert = aws.acm.Certificate { domain_name = 'main' }\n";

#[test]
fn deferred_provider_annotations_use_exact_parser_tokens_in_the_owner_file() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider broken {}\nlet main_decoy = 'SharedId'\n",
        ),
        (
            "sibling.crn",
            "let decoy = '😀 SharedId'\nattributes { value: SharedId = 'id' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine()
            .with_provider_errors(HashMap::from([("broken".into(), "unavailable".into())])),
        "type check for SharedId was deferred",
        &[("SharedId", 1)],
    );
}

#[test]
fn deferred_function_checks_preserve_annotation_and_complete_call_locations() {
    let directory = Directory::new(&[
        ("main.crn", "provider broken {}\nlet unrelated = 'main'\n"),
        (
            "sibling.crn",
            "let decoy = '😀 Missing echo'\nfn echo(value: Missing) { value }\nlet actual = echo('id')\n",
        ),
    ]);
    let engine = test_engine()
        .with_provider_errors(HashMap::from([("broken".into(), "unavailable".into())]));
    directory.assert_sibling_owner(
        &engine,
        "type check for Missing was deferred",
        &[("Missing", 1), ("echo('id')", 0)],
    );
}

#[test]
fn unused_function_arithmetic_uses_the_operator_in_its_owner_file() {
    let directory = Directory::new(&[
        ("main.crn", "let decoy = '😀 +'\n"),
        (
            "sibling.crn",
            "let decoy = '😀 +'\nfn bad(value: String): Int { value + 1 }\n",
        ),
    ]);
    directory.assert_sibling_owner(&test_engine(), "left operand must be Int", &[("+", 1)]);
}

#[test]
fn resource_arithmetic_preserves_each_authored_operator() {
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\nlet main_decoy = '+'\n"),
        (
            "sibling.crn",
            "arguments { text: String }\nlet decoy = '😀 +'\nmock.test.Thing { enabled = text + 1 }\nmock.test.Thing { enabled = text + 2 }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "left operand must be Int",
        &[("+", 1), ("+", 2)],
    );
}

#[test]
fn substituted_integer_operator_keeps_its_file_when_coordinates_coincide() {
    use carina_core::parser::AnalysisTree;
    let source = "arguments { ratio: Float }\nmock.test.Thing { enabled = ratio + 1 }\n";
    let directory = Directory::new(&[("main.crn", source), ("sibling.crn", source)]);
    let main = directory.root.path().join("main.crn");
    let sibling = directory.root.path().join("sibling.crn");
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut consumer = carina_core::parser::parse_file(source, &main, &catalog).unwrap();
    let authored = carina_core::parser::parse_file(source, &sibling, &catalog).unwrap();
    // Expansion substitutes a value, retaining its own parser provenance.
    consumer.resources[0].attributes["enabled"] =
        authored.resources[0].attributes["enabled"].clone();
    let tree = AnalysisTree::try_from_file(&consumer).unwrap();
    let resource = tree.resources().next().unwrap();
    let engine = resource_location_schema_engine();
    let schema = engine
        .schemas
        .get(
            "mock",
            "test.Thing",
            carina_core::schema::SchemaKind::Resource,
        )
        .unwrap();
    let bindings =
        carina_core::binding_index::BindingIndex::from_parsed(&consumer, &engine.schemas, &catalog);
    let findings = tree.integer_arithmetic_findings(
        resource,
        "enabled",
        schema.effective_attribute("enabled").unwrap().value_type(),
        &carina_core::validation::inference::bindings_from_parsed(&consumer),
        &engine.schemas,
        &bindings,
    );
    assert_eq!(findings.len(), 1, "{findings:?}");
    let finding = &findings[0];
    assert_eq!(finding.location().file().path(), sibling);
    let span = finding.location().span();
    assert_eq!(&source[span.start_byte()..span.end_byte()], "+");
    assert!(
        render_merged(
            &create_document_at(source, main),
            finding,
            DiagnosticSeverity::ERROR,
            finding.detail().to_string()
        )
        .is_none()
    );
    let rendered = render_merged(
        &create_document_at(source, sibling),
        finding,
        DiagnosticSeverity::ERROR,
        finding.detail().to_string(),
    )
    .unwrap();
    let column = source.lines().nth(1).unwrap().find('+').unwrap() as u32;
    assert_eq!(
        rendered.range,
        Range::new(Position::new(1, column), Position::new(1, column + 1))
    );
}

#[test]
fn undefined_references_use_each_parser_occurrence_and_ignore_decoys() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider mock {}\nlet main_decoy = 'missing.value'\n",
        ),
        (
            "sibling.crn",
            "let decoy = '😀 missing.value'\nmock.test.Thing { enabled = missing.value }\nmock.test.Thing { enabled = missing.value }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "Undefined resource: 'missing'",
        &[("missing.value", 1), ("missing.value", 2)],
    );
}

#[test]
fn authored_config_label_does_not_render_in_unrelated_buffers() {
    use carina_core::validation::deferred_types::{ProviderTypeCheckSite, TypeCheckSession};
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let source = "attributes { value: Missing = 'id' }";
    let parsed = carina_core::parser::parse_file(source, Path::new("<config>"), &catalog).unwrap();
    let terminal = TypeCheckSession::run(&catalog, |session| {
        session.record(
            &carina_core::parser::TypeExpr::String,
            ProviderTypeCheckSite::new(
                parsed.attribute_params[0].type_check_location().clone(),
                "authored check",
            ),
        );
        Vec::new()
    });
    let doc = create_document_at(source, "/project/sibling.crn".into());
    assert!(DiagnosticEngine::publish_type_diagnostics(terminal, &doc).is_empty());
}

#[test]
fn unloaded_provider_notice_ignores_comment_and_string_decoys() {
    let source = "// provider aws {}\nlet decoy = 'provider aws {}'\nprovider aws {}\n";
    let directory = Directory::new(&[
        ("main.crn", "let main = 'provider aws {}'\n"),
        ("sibling.crn", source),
    ]);
    let engine =
        test_engine().with_provider_errors(HashMap::from([("aws".into(), "unavailable".into())]));
    directory.assert_sibling_owner(&engine, "Provider 'aws' is not loaded", &[("aws", 2)]);
}

fn resource_location_schema_engine() -> DiagnosticEngine {
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    };
    let schema = ResourceSchema::new("test.Thing")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("enabled", AttributeType::bool()).build())
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::enum_(
                    TypeIdentity::bare("Mode"),
                    Some(vec!["fast".into(), "slow".into()]),
                    vec![],
                    None,
                    None,
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "config",
                AttributeType::struct_(
                    "Config",
                    vec![StructField::optional("enabled", AttributeType::bool()).build()],
                ),
            )
            .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    custom_engine(schemas)
}

fn assert_resource_location(body: &str, message: &str, tokens: &[(&str, usize)]) {
    // Both the buffer parse and the sibling-function semantic fallback carry
    // the same authored spans. Test them with inline syntax and decoy text.
    for iterable in ["['one']", "names()"] {
        let source = format!(
            "let decoy = '😀 mock.test.Missing enabled = mode = config typo'\nlet items = for item in {iterable} {{ {body} }}\n"
        );
        let directory = Directory::new(&[
            (
                "main.crn",
                "provider mock {}\nfn names(): list(String) { ['one'] }\n",
            ),
            ("sibling.crn", &source),
        ]);
        let doc = create_document_at(&source, directory.root.path().join("sibling.crn"));
        assert_eq!(
            doc.parsed().is_none(),
            iterable == "names()",
            "{iterable}: {:?}",
            doc.parse_error()
        );
        directory.assert_sibling_owner(&resource_location_schema_engine(), message, tokens);
    }
}

#[test]
fn resource_type_uses_parser_header() {
    assert_resource_location(
        "mock.test.Missing {}",
        "No schema for mock.test.Missing",
        &[("mock.test.Missing", 1)],
    );
}

#[test]
fn attribute_type_uses_parser_key() {
    assert_resource_location(
        "mock.test.Thing { enabled = 'bad' }",
        "expected Bool",
        &[("enabled", 1)],
    );
}

#[test]
fn enum_uses_parser_value() {
    assert_resource_location(
        "mock.test.Thing { mode = 'bad' }",
        "string literal",
        &[("'bad'", 0)],
    );
}

#[test]
fn unknown_attribute_uses_parser_assignment() {
    assert_resource_location(
        "mock.test.Thing { typo = true }",
        "Unknown attribute",
        &[("typo = true", 0)],
    );
}

#[test]
fn multiline_unknown_attribute_keeps_the_precise_parser_key() {
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\n"),
        (
            "sibling.crn",
            "let decoy = '😀 typo'\nmock.test.Thing {\n  typo = [\n    'value'\n  ]\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &resource_location_schema_engine(),
        "Unknown attribute 'typo'",
        &[("typo", 1)],
    );
}

#[test]
fn block_syntax_uses_each_parser_key() {
    assert_resource_location(
        "mock.test.Thing { config { enabled = true } }",
        "cannot use block syntax",
        &[("config", 1)],
    );
}

#[test]
fn nested_schema_uses_parser_field() {
    assert_resource_location(
        "mock.test.Thing { config = { typo = true } }",
        "Unknown field",
        &[("typo", 1)],
    );
}

#[test]
fn degraded_duplicates_keep_only_buffer_findings() {
    let directory = Directory::new(&[
        ("main.crn", "let same = 'first'\n"),
        (
            "sibling.crn",
            "let same = provider mock {}\nlet same = provider mock {}\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_ranges(&engine, "main.crn", "duplicate binding", &[]);
    directory.assert_ranges(&engine, "sibling.crn", "duplicate binding", &[("same", 1)]);
}

#[test]
fn module_call_findings_are_reported_once() {
    let directory = Directory::new(&[
        ("main.crn", "let service = use { source = './module' }\n"),
        ("sibling.crn", "service {}\n"),
        ("module/main.crn", "arguments { required_name: String }\n"),
    ]);
    let path = directory.root.path().join("sibling.crn");
    let doc = create_document_at("service {}\n", path.clone());
    let engine = test_engine();
    let findings = engine.analyze(&doc, Some(&path));
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.message.contains("required_name"))
            .count(),
        1,
        "{findings:#?}"
    );
    directory.assert_sibling_owner(&engine, "required_name", &[("service {}", 0)]);
}

#[test]
fn cached_module_findings_keep_upstream_type_authority() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let up = upstream_state { source = './upstream' }\nlet service = use { source = './module' }\nlet broken = use { source = './missing' }\nbroken {}\n",
        ),
        ("sibling.crn", "service { count = up.value }\n"),
        ("module/main.crn", "arguments { count: Int }\n"),
        ("upstream/main.crn", "exports { value: String = 'wrong' }\n"),
    ]);
    directory.assert_sibling_owner(&test_engine(), "expected Int", &[("up.value", 0)]);
}

#[test]
fn cycle_reports_once_per_member_file_in_ring_order() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let a = aws.acm.Certificate { domain_name = 'a' directives { depends_on = [c] } }\n",
        ),
        (
            "values.crn",
            "let b = aws.acm.Certificate { domain_name = a.domain_name }\nlet c = aws.acm.Certificate { domain_name = b.domain_name }\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    let message = "Circular dependency detected: a -> c -> b -> a";
    directory.assert_sibling_owner(&engine, message, &[("c", 3)]);
    directory.assert_ranges(&engine, "values.crn", message, &[("b.domain_name", 0)]);
}

#[test]
fn wait_does_not_resolve_deferred_if_target() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\narguments { enabled: Bool }\nlet selected = if enabled { aws.acm.Certificate { domain_name = 'conditional' } }\n",
        ),
        (
            "sibling.crn",
            "let ready = wait selected { until = selected.status == 'ISSUED' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "target binding `selected` is not a known resource",
        &[("selected", 0)],
    );
}

#[test]
fn type_resolution_invariant_renders_in_each_directory_buffer() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let other = aws.acm.Certificate { domain_name = 'sibling' }\n",
        ),
    ]);
    let config =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut parsed = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
        &config,
    )
    .unwrap()
    .into_parsed();
    parsed.resources[0].declaration = Default::default();
    let error =
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            carina_core::validation::resolve_file_type_exprs_report(&mut parsed, session, None)
        })
        .expect_err("return the typed boundary failure");
    for name in ["main.crn", "sibling.crn"] {
        let path = directory.root.path().join(name);
        let source = std::fs::read_to_string(&path).unwrap();
        let doc = create_document_at(&source, path);
        let diagnostic = render_analysis_invariant(&doc, &error).unwrap();
        assert_eq!(diagnostic.range, Range::default());
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostic
                .message
                .contains("missing declaration provenance")
        );
    }
}

#[test]
fn failed_function_annotation_keeps_main_finding_set() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nprovider broken { source = 'file:///definitely-missing/broken.wasm' }\n",
        ),
        (
            "sibling.crn",
            "fn bad(x: aws.Missing) { x }\nfn echo(x: Arn): String { 'ok' }\nexports { failed: String = bad('id') x = echo('id') }\n",
        ),
    ]);
    let engine =
        test_engine_with_wait_target().with_provider_errors(std::collections::HashMap::from([(
            "broken".to_string(),
            "fixture provider unavailable".to_string(),
        )]));
    directory.assert_sibling_owner(
        &engine,
        "unknown custom type 'aws.Missing'",
        &[("aws.Missing", 0)],
    );
    directory.assert_sibling_owner(&engine, "export 'failed'", &[]);
}

#[test]
fn merged_function_error_uses_parser_owned_body_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet cert = aws.acm.Certificate { domain_name = 'main' }\nlet invalid = identity(cert)\n",
        ),
        (
            "sibling.crn",
            "# identity is called from main.crn\nfn identity(r: aws.acm.Certificate): String { r }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "'r' is a resource handle, not a value",
        &[("r", 5)],
    );
}

#[test]
fn module_function_error_keeps_its_element_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet child = use { source = './module' }\nlet instance = child {}\n",
        ),
        ("sibling.crn", "let unrelated = 'sibling'\n"),
        (
            "module/arguments.crn",
            "arguments { unused: Bool = true }\n",
        ),
        (
            "module/functions.crn",
            "fn identity(resource_argument: aws.acm.Certificate): String { resource_argument }\n",
        ),
        (
            "module/resources.crn",
            "let cert = aws.acm.Certificate { domain_name = 'module' }\nlet invalid = identity(cert)\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    let message = "'resource_argument' is a resource handle, not a value";
    directory.assert_ranges(&engine, "main.crn", message, &[("child", 1)]);
    directory.assert_ranges(&engine, "sibling.crn", message, &[]);
    directory.assert_ranges(
        &engine,
        "module/functions.crn",
        message,
        &[("resource_argument", 1)],
    );
}

#[test]
fn provider_error_uses_sibling_attribute_location() {
    let directory = Directory::new(&[
        ("main.crn", "let regional = provider aws {}\n"),
        ("sibling.crn", "provider aws { default_tags = 'wrong' }\n"),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "default_tags must resolve to a map",
        &[("'wrong'", 0)],
    );
}

#[test]
fn expanded_provider_invariant_is_rendered() {
    use carina_core::parser::{AnalysisResolutionError, ProviderContext};
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\n"),
        ("sibling.crn", "let unrelated = 'sibling'\n"),
    ]);
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut expanded = carina_core::config_loader::parse_directory(
        &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
        &config,
    )
    .unwrap()
    .into_parsed()
    .resolve_root_for_test();
    expanded.providers.push(expanded.providers[0].clone());
    let error = carina_core::parser::resolve_expanded_provider_unresolved_attributes_for_analysis(
        &mut expanded,
        &config,
    )
    .unwrap_err();
    assert!(matches!(&error, AnalysisResolutionError::Invariant(_)));
    for name in ["main.crn", "sibling.crn"] {
        let source = std::fs::read_to_string(directory.root.path().join(name)).unwrap();
        let doc = create_document_at(&source, directory.root.path().join(name));
        let diagnostic = render_analysis_resolution_error(&doc, &error)
            .expect("surface the directory invariant");
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("provider aws declaration"));
    }
}

#[test]
fn routing_waits_for_the_directory_parse() {
    let directory = Directory::new(&[
        ("main.crn", "let regional = provider aws {}\n"),
        (
            "sibling.crn",
            "aws.acm.Certificate { domain_name = 'sibling' directives { provider = regional } }\n",
        ),
        ("broken.crn", "invalid {{{"),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "not a known provider instance",
        &[],
    );
}

#[test]
fn unmappable_span_drops_only_that_finding() -> Result<(), carina_core::parser::AnalysisTreeError> {
    let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", "let live = 'live'\n")]);
    let path = directory.root.path().join("sibling.crn");
    let document = create_document_at("let live = 'live'\n", path.clone());
    let stale_source = format!("{}let stale = 'disk'\n", "\n".repeat(80));
    let stale = create_document_at(&stale_source, path);
    let live_tree = carina_core::parser::AnalysisTree::try_from_file(document.parsed().unwrap())?;
    let stale_tree = carina_core::parser::AnalysisTree::try_from_file(stale.parsed().unwrap())?;
    let findings = [
        &stale_tree.syntax().declarations()[0],
        &live_tree.syntax().declarations()[0],
    ];
    let diagnostics: Vec<_> = findings
        .into_iter()
        .filter_map(|finding| {
            render_merged(
                &document,
                finding,
                DiagnosticSeverity::ERROR,
                "test finding".to_string(),
            )
        })
        .collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 4), Position::new(0, 8))
    );
    Ok(())
}

#[test]
fn module_reload_uses_the_unsaved_buffer() {
    let directory = Directory::new(&[
        ("main.crn", "invalid {{{"),
        (
            "sibling.crn",
            "arguments { label: String = 'default' }\nattributes { name = label }\n",
        ),
    ]);
    let path = directory.root.path().join("main.crn");
    // Loading this directory as a module must see the live self-import and
    // report its cycle, rather than parsing the obsolete on-disk syntax.
    let document = create_document_at(
        "let local = use { source = '.' }\nlet call = local {}\n",
        path.clone(),
    );
    let merged = test_engine()
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(
                &document,
                Some("main.crn"),
                &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
            )
        })
        .unwrap();
    let errors = match &merged.module_resolution {
        checks::MergedModuleResolution::Unanalyzed => panic!("module analysis must run"),
        checks::MergedModuleResolution::Unowned { errors }
        | checks::MergedModuleResolution::ConstraintFailed { errors, .. } => errors,
        checks::MergedModuleResolution::Expanded { .. } => panic!("self-import must fail"),
    };
    let messages: Vec<_> = errors
        .iter()
        .map(|error| error.display_relative_to(directory.root.path()))
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("ircular import")),
        "{messages:#?}"
    );
    assert!(
        !messages.iter().any(|message| message.contains("invalid")),
        "{messages:#?}"
    );
}

#[test]
fn upstream_reload_uses_the_unsaved_buffer() {
    let directory = Directory::new(&[
        ("main.crn", "invalid {{{"),
        ("sibling.crn", "let label = 'sibling'\n"),
    ]);
    directory.assert_buffer_ranges(
        &test_engine(),
        "main.crn",
        "let prior = upstream_state { source = '.' }\nexports { value: String = 'live' }\n",
        "failed to parse",
        &[],
    );
}

#[test]
fn unreadable_sibling_keeps_buffer_duplicates() {
    let source = "exports { repeated: String = 'one' repeated: String = 'two' }\n";
    let directory = Directory::new(&[
        ("main.crn", "let unrelated = 'main'\n"),
        ("sibling.crn", "let on_disk = 'old'\n"),
    ]);
    std::fs::write(directory.root.path().join("broken.crn"), [0xff]).unwrap();
    directory.assert_ranges(&test_engine(), "main.crn", "duplicate export", &[]);
    directory.assert_buffer_ranges(
        &test_engine(),
        "sibling.crn",
        source,
        "duplicate export",
        &[("repeated", 0)],
    );
}

#[test]
fn function_refs_use_each_authored_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let decoy = 'cert.domain_name'\n# cert.domain_name\nfn returns_int(): Int { cert.domain_name }\nfn another_int(): Int { cert.domain_name }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "expected Int, got String",
        &[("cert.domain_name", 2), ("cert.domain_name", 3)],
    );
}

#[test]
fn resource_refs_ignore_decoys_and_keep_each_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "# cert.statu\naws.acm.Certificate { domain_name = cert.statu }\naws.acm.Certificate { domain_name = cert.statu }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Unknown attribute 'statu'",
        &[("statu", 1), ("statu", 2)],
    );
}

#[test]
fn non_bmp_positions_agree_across_renderers() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀' let values = map('.name', [{ name = 'one' }])\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Consider using pipe form",
        &[("map", 0)],
    );
    assert_eq!(position::byte_offset_to_char_offset("😀x", 4), 2);
    assert_eq!(position::char_len("😀x"), 3);
    let source = std::fs::read_to_string(directory.root.path().join("sibling.crn")).unwrap();
    let tokens = crate::semantic_tokens::SemanticTokensProvider::new(&[]).tokenize(&source);
    let mut column = 0;
    let operators: Vec<_> = tokens
        .iter()
        .filter_map(|token| {
            column = if token.delta_line == 0 {
                column + token.delta_start
            } else {
                token.delta_start
            };
            (token.token_type == 6).then_some(column)
        })
        .collect();
    let expected: Vec<_> = source
        .match_indices('=')
        .map(|(byte, _)| position::byte_offset_to_char_offset(&source, byte))
        .collect();
    assert_eq!(
        operators, expected,
        "semantic tokens must agree with diagnostic UTF-16 columns"
    );

    let path = directory.root.path().join("sibling.crn");
    let mut document = create_document_at(&source, path);
    let start = source.find("values").unwrap();
    document.apply_change(tower_lsp::lsp_types::TextDocumentContentChangeEvent {
        range: Some(Range::new(
            Position::new(0, position::byte_offset_to_char_offset(&source, start)),
            Position::new(
                0,
                position::byte_offset_to_char_offset(&source, start + "values".len()),
            ),
        )),
        range_length: None,
        text: "updated".into(),
    });
    assert_eq!(document.text(), source.replace("values", "updated"));
}

#[test]
fn parser_ranges_use_utf16() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀' aws.acm.Certificate { domain_name = 'first' domain_name = 'second' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Duplicate attribute 'domain_name'",
        &[("domain_name", 1)],
    );
}

#[test]
fn depends_on_belongs_to_sibling_unnamed_resource() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = '😀 missing'\n  directives { depends_on = [\n    missing\n  ] }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "binding 'missing' is not declared",
        &[("missing", 1)],
    );
}

#[test]
fn depends_on_duplicate_anchors_second_occurrence() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let consumer = aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { depends_on = [cert,\n    cert, cert] }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "listed multiple times",
        &[("cert", 1)],
    );
}

#[test]
fn depends_on_belongs_to_sibling_for_body() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let items = for item in [1] {\n  aws.acm.Certificate {\n    domain_name = 'loop'\n    directives { depends_on = [missing] }\n  }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "binding 'missing' is not declared",
        &[("missing", 0)],
    );
}

#[test]
fn wait_target_belongs_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let ready = wait absent {\n  until = absent.status == 'ready'\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "not a known resource",
        &[("absent", 0)],
    );
}

#[test]
fn wait_operands_belong_to_sibling() {
    let engine = test_engine_with_wait_target();
    for (source, message, token) in [
        (
            "let ready = wait cert {\n  until =\n    cert.statu == 'ready'\n}\n",
            "unknown attribute",
            "cert.statu",
        ),
        (
            "let ready = wait cert {\n  until = cert.status ==\n    ready\n}\n",
            "cannot resolve `ready`",
            "ready",
        ),
        (
            "let ready = wait cert {\n  until = cert.status == 'ready'\n  depends_on = [\n    missing\n  ]\n}\n",
            "binding 'missing' is not declared",
            "missing",
        ),
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        let occurrence = usize::from(message == "cannot resolve `ready`");
        directory.assert_sibling_owner(&engine, message, &[(token, occurrence)]);
    }
}

#[test]
fn provider_routing_belongs_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { provider = typo }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn repeated_provider_directives_anchor_the_effective_operand() {
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\nlet valid = provider aws {}\n"),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'consumer'\n  directives { provider = valid provider = typo }\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn cycle_belongs_only_to_members_and_preserves_value_edge() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet independent = aws.acm.Certificate { domain_name = 'unrelated' }\n",
        ),
        (
            "sibling.crn",
            "let a = aws.acm.Certificate {\n  domain_name = 'a'\n  directives { depends_on = [b] }\n}\n",
        ),
        (
            "values.crn",
            "let b = aws.acm.Certificate {\n  domain_name = a.domain_name\n}\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_sibling_owner(&engine, "Circular dependency", &[("b", 0)]);
    directory.assert_ranges(
        &engine,
        "values.crn",
        "Circular dependency",
        &[("a.domain_name", 0)],
    );
}

fn deferred_engine() -> DiagnosticEngine {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        ResourceSchema::new("acm.Certificate")
            .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("status", AttributeType::string())
                    .deferred_populate()
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    DiagnosticEngine::new(loaded_test_providers(schemas), vec!["aws".into()])
}

#[test]
fn deferred_populate_belongs_to_sibling_attribute() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = cert.status\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &deferred_engine(),
        "populated asynchronously",
        &[("domain_name", 0)],
    );
}

#[test]
fn repeated_attribute_keys_anchor_the_effective_value() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate {\n  domain_name = 'overwritten'\n  domain_name = cert.status\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &deferred_engine(),
        "populated asynchronously",
        &[("domain_name", 1)],
    );
}

#[test]
fn deferred_if_body_diagnostics_belong_to_sibling() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        ("arguments.crn", "arguments { enabled: Bool }\n"),
        (
            "sibling.crn",
            "let selected = if enabled {\n  aws.acm.Certificate {\n    domain_name = cert.status\n    directives { provider = typo depends_on = [missing] }\n  }\n}\n",
        ),
    ]);
    let engine = deferred_engine();
    directory.assert_sibling_owner(&engine, "populated asynchronously", &[("domain_name", 0)]);
    directory.assert_sibling_owner(
        &engine,
        "binding 'missing' is not declared",
        &[("missing", 0)],
    );
    directory.assert_sibling_owner(
        &engine,
        "`typo` is not a known provider instance",
        &[("typo", 0)],
    );
}

#[test]
fn expanded_module_diagnostic_keeps_its_authored_file()
-> Result<(), carina_core::parser::AnalysisTreeError> {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet module = use { source = './module' }\nlet instance = module {}\n",
        ),
        (
            "module/arguments.crn",
            "arguments { label: String = 'module' }\n",
        ),
        (
            "module/sibling.crn",
            "let cert = aws.acm.Certificate { domain_name = 'module' }\nlet job = aws.acm.Certificate { domain_name = 'job' }\nlet ready = wait job { until = job.statu == 'ready' }\naws.acm.Certificate {\n  domain_name = cert.status\n  directives { depends_on = [cert, cert] }\n}\n",
        ),
    ]);
    let engine = deferred_engine();
    let root_path = directory.root.path().join("main.crn");
    let root_source = std::fs::read_to_string(&root_path).unwrap();
    let document = create_document_at(&root_source, root_path);
    let merged = engine
        .with_run_for_test(|run| {
            run.parse_and_resolve_merged_with_buffer(
                &document,
                Some("main.crn"),
                &carina_core::paths::ResolvedPath::new(directory.root.path()).unwrap(),
            )
        })
        .unwrap();
    let crate::diagnostics::checks::MergedModuleResolution::Expanded { graph } =
        &merged.module_resolution
    else {
        panic!("fixture must exercise a genuinely expanded module")
    };
    let tree = carina_core::parser::AnalysisTree::try_from_file(graph.as_ref())?;
    let waits = engine.with_run_for_test(|run| {
        carina_core::validation::wait::validate_wait_bindings(&tree, &run.schemas)
    });
    assert_eq!(waits.len(), 1);
    assert_eq!(
        waits[0].location().file().path(),
        directory.root.path().join("module/sibling.crn")
    );
    directory.assert_ranges(&engine, "main.crn", "populated asynchronously", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "populated asynchronously",
        &[("domain_name", 2)],
    );
    directory.assert_ranges(&engine, "main.crn", "unknown attribute", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "unknown attribute",
        &[("job.statu", 0)],
    );
    directory.assert_ranges(&engine, "main.crn", "listed multiple times", &[]);
    directory.assert_ranges(
        &engine,
        "module/sibling.crn",
        "listed multiple times",
        &[("cert", 3)],
    );
    Ok(())
}

#[test]
fn deferred_populate_and_routing_in_static_and_deferred_for_bodies_keep_the_owner() {
    for iterable in ["['item']", "names"] {
        let source = format!(
            "let items = for item in {iterable} {{\n  aws.acm.Certificate {{\n    domain_name = cert.status\n    directives {{ provider = typo }}\n  }}\n}}\n"
        );
        let directory = Directory::new(&[
            ("main.crn", MAIN),
            ("arguments.crn", "arguments { names: list(String) }\n"),
            ("sibling.crn", &source),
        ]);
        let engine = deferred_engine();
        directory.assert_sibling_owner(&engine, "populated asynchronously", &[("domain_name", 0)]);
        directory.assert_sibling_owner(
            &engine,
            "`typo` is not a known provider instance",
            &[("typo", 0)],
        );
    }
}

#[test]
fn unsaved_sibling_override_uses_its_current_parser_span() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "aws.acm.Certificate { domain_name = 'on disk' }\n",
        ),
    ]);
    let source = "# unsaved 😀\n\naws.acm.Certificate {\n  domain_name = 'current'\n  directives { depends_on = [missing] }\n}\n";
    let path = directory.root.path().join("sibling.crn");
    let engine = test_engine_with_wait_target();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    let matching: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("binding 'missing' is not declared")
        })
        .collect();
    assert_eq!(matching.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        matching[0].range,
        Range::new(Position::new(4, 29), Position::new(4, 36))
    );
}

#[test]
fn cycle_from_exports_uses_the_sibling_lets_reference_location() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet a = aws.acm.Certificate { domain_name = 'a' directives { depends_on = [b] } }\nlet value = a.domain_name\n",
        ),
        (
            "exports.crn",
            "let b = aws.acm.Certificate { domain_name = value }\nexports { result = b.domain_name }\n",
        ),
        ("unrelated.crn", "let unrelated = 'unrelated'\n"),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_ranges(
        &engine,
        "main.crn",
        "Circular dependency",
        &[("a.domain_name", 0)],
    );
    directory.assert_ranges(&engine, "exports.crn", "Circular dependency", &[]);
    directory.assert_ranges(&engine, "unrelated.crn", "Circular dependency", &[]);
}

#[test]
fn depends_on_precise_anchors_ignore_strings_comments_and_other_resources() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let first = aws.acm.Certificate {\n  domain_name = 'depends_on = [missing]'\n  # depends_on = [missing]\n  directives { depends_on = [missing] }\n}\nlet second = aws.acm.Certificate {\n  domain_name = 'other'\n  directives { depends_on = [another] }\n}\n",
        ),
    ]);
    let engine = test_engine_with_wait_target();
    directory.assert_sibling_owner(
        &engine,
        "binding 'missing' is not declared",
        &[("missing", 2)],
    );
    directory.assert_sibling_owner(
        &engine,
        "binding 'another' is not declared",
        &[("another", 0)],
    );
}

#[test]
fn upstream_resolve_owns_each_recorded_source() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let other = upstream_state { source = './broken' }\n",
        ),
        (
            "sibling.crn",
            "let orgs = upstream_state {\n  source =\n    './broken'\n}\n",
        ),
        ("broken/main.crn", "invalid {{{"),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "upstream_state `orgs`: failed to parse",
        &[("'./broken'", 0)],
    );
}

#[test]
fn upstream_field_owns_repeated_occurrences_after_unicode_heredoc() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.missing'\n",
        ),
        (
            "sibling.crn",
            "let note = <<TEXT\n😀 orgs.missing\nTEXT\n# orgs.missing\nlet first = orgs.missing\nlet second = orgs.missing\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "does not export `missing`",
        &[("orgs.missing", 2), ("orgs.missing", 3)],
    );
}

#[test]
fn upstream_for_shape_owns_multiline_iterable() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "provider aws {}\nlet orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "let note = '😀 orgs.accounts'\nfor item in\n  orgs.accounts\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: map(String) = { a = 'a' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "is declared as",
        &[("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_attribute_shape_owns_reference_prefix() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "exports {\n  first = orgs.accounts.name\n  second = orgs.accounts.name\n}\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "reads it as a struct",
        &[("orgs.accounts", 0), ("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_subscript_shape_owns_reference_prefix() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet decoy = 'orgs.accounts'\n",
        ),
        (
            "sibling.crn",
            "exports {\n  first = orgs.accounts['bad']\n  second = orgs.accounts['bad']\n}\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "subscript",
        &[("orgs.accounts", 0), ("orgs.accounts", 1)],
    );
}

#[test]
fn upstream_field_in_main_is_not_reanchored_at_export_alias() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\nlet bad = orgs.missing\n",
        ),
        ("exports.crn", "exports { result = bad }\n# orgs.missing\n"),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_ranges(&engine, "exports.crn", "does not export `missing`", &[]);
    directory.assert_ranges(
        &engine,
        "main.crn",
        "does not export `missing`",
        &[("orgs.missing", 0)],
    );
}

#[test]
fn upstream_field_unsaved_override_owns_new_occurrences() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\n",
        ),
        ("sibling.crn", "let old = orgs.missing\n"),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    let engine = test_engine();
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "# orgs.missing\nlet note = '😀'\nlet edited = orgs.missing\n",
        "does not export `missing`",
        &[("orgs.missing", 1)],
    );
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let fixed = orgs.accounts\n",
        "does not export `missing`",
        &[],
    );
}

#[test]
fn remaining_type_resolution_owns_annotation_tokens() {
    for source in [
        "arguments { value:\n  TotallyMadeUpType }\n",
        "attributes { value:\n  TotallyMadeUpType = 'x' }\n",
        "exports { value:\n  TotallyMadeUpType = 'x' }\n",
        "fn example(value:\n  TotallyMadeUpType) { value }\n",
        "fn example(value: String):\n  TotallyMadeUpType { value }\n",
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        directory.assert_sibling_owner(
            &test_engine_with_wait_target(),
            "unknown custom type",
            &[("TotallyMadeUpType", 0)],
        );
    }
}

#[test]
fn remaining_deferred_iterable_type_owns_multiline_expression() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "arguments { count: Int }\nfor item in\n  count\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "expected list iterable, got Int",
        &[("count", 1)],
    );
}

#[test]
fn remaining_inference_failure_owns_inline_export_name() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let note = '😀'\nexports { result = [1, 'a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "type annotation required",
        &[("result", 0)],
    );
}

#[test]
fn remaining_duplicate_bindings_keep_last_occurrence_per_file() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "let same = '😀'\nlet same = 'second'\nlet same = 'third'\n",
        ),
    ]);
    directory.assert_sibling_owner(&test_engine(), "same'", &[("same", 2)]);
}

#[test]
fn remaining_duplicate_declarations_keep_occurrences_across_files() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "exports { same = 'first' same = 'second' }\n",
        ),
        ("exports.crn", "exports { same = 'third' }\n"),
    ]);
    let engine = test_engine();
    directory.assert_sibling_owner(&engine, "same'", &[("same", 0)]);
    directory.assert_ranges(&engine, "exports.crn", "same'", &[("same", 0)]);
}

#[test]
fn remaining_unknown_iterable_root_owns_recorded_reference() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        (
            "sibling.crn",
            "for item in\n  missing.items\n{ aws.acm.Certificate { domain_name = 'loop' } }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "Undefined identifier `missing`",
        &[("missing.items", 0)],
    );
}

#[test]
fn remaining_missing_upstream_directory_owns_source_value() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let other = upstream_state { source = './missing' }\n",
        ),
        (
            "sibling.crn",
            "let orgs = upstream_state {\n  source =\n    './missing'\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "upstream_state 'orgs': source",
        &[("'./missing'", 0)],
    );
}

#[test]
fn upstream_field_utf16_columns_after_heredoc_and_astral_literal() {
    let directory = Directory::new(&[
        (
            "main.crn",
            "let orgs = upstream_state { source = './upstream' }\n",
        ),
        (
            "sibling.crn",
            "let note = <<TEXT\n😀 orgs.missing\nTEXT\nlet values = ['😀', orgs.missing, orgs.missing]\n",
        ),
        (
            "upstream/exports.crn",
            "exports { accounts: list(String) = ['a'] }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "does not export `missing`",
        &[("orgs.missing", 1), ("orgs.missing", 2)],
    );
}

#[test]
fn remaining_duplicate_inventory_covers_all_declaration_kinds() {
    for (source, name, message, occurrence) in [
        (
            "let repeated = 'a'\nlet repeated = 'b'\n",
            "repeated",
            "duplicate binding",
            1,
        ),
        (
            "arguments { repeated: String repeated: String }\n",
            "repeated",
            "duplicate argument",
            0,
        ),
        (
            "attributes { repeated = 'a' repeated = 'b' }\n",
            "repeated",
            "duplicate module attribute",
            0,
        ),
        (
            "exports { repeated = 'a' repeated = 'b' }\n",
            "repeated",
            "duplicate export",
            0,
        ),
        (
            "fn repeated() { 'a' }\nfn repeated() { 'b' }\n",
            "repeated",
            "duplicate function",
            0,
        ),
        (
            "provider mock {}\nprovider mock {}\n",
            "mock",
            "duplicate provider kind",
            0,
        ),
    ] {
        let directory = Directory::new(&[("main.crn", MAIN), ("sibling.crn", source)]);
        directory.assert_sibling_owner(
            &test_engine_with_wait_target(),
            message,
            &[(name, occurrence)],
        );
    }
}

#[test]
fn remaining_duplicate_inventory_honors_unsaved_override() {
    let directory = Directory::new(&[
        ("main.crn", MAIN),
        ("sibling.crn", "let repeated = 'a'\nlet repeated = 'b'\n"),
    ]);
    let engine = test_engine();
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let fixed = '😀'\n",
        "duplicate binding",
        &[],
    );
    directory.assert_buffer_ranges(
        &engine,
        "sibling.crn",
        "let note = '😀'\nlet repeated = 'a'\nlet repeated = 'b'\n",
        "duplicate binding",
        &[("repeated", 1)],
    );
}

#[test]
fn remaining_provider_finalization_owns_recorded_attribute_value() {
    let directory = Directory::new(&[
        ("main.crn", "let tag_values = 'bad'\n"),
        (
            "sibling.crn",
            "provider aws {\n  default_tags =\n    tag_values\n}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine_with_wait_target(),
        "default_tags must resolve to a map",
        &[("tag_values", 0)],
    );
}

#[test]
fn closure_module_placement_owns_multiline_provider_header() {
    let directory = Directory::new(&[
        ("main.crn", "arguments { label: String = 'module' }\n"),
        (
            "sibling.crn",
            "let note = <<TEXT\nprovider aws {}\nTEXT\nprovider\n  aws {}\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "provider blocks are not allowed inside modules",
        &[("provider\n  aws", 0)],
    );
}

#[test]
fn closure_arguments_in_root_owns_keyword_after_heredoc() {
    let directory = Directory::new(&[
        ("main.crn", "provider aws {}\n"),
        (
            "sibling.crn",
            "let note = <<TEXT\narguments {\nTEXT\narguments\n{ label: String = 'module' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "arguments blocks are only valid inside module definitions",
        &[("arguments", 1)],
    );
}

#[test]
fn closure_backend_attribute_owns_utf16_key() {
    let directory = Directory::new(&[
        ("main.crn", "let label = 'main'\n"),
        (
            "sibling.crn",
            "let note = '😀' backend local { path = 'state.json' typo = 'x' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &test_engine(),
        "Unknown attribute 'typo' for backend type 'local'",
        &[("typo", 0)],
    );
}

#[test]
fn closure_anonymous_identity_owns_utf16_resource_headers() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Thing")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let directory = Directory::new(&[
        ("main.crn", "provider mock {}\n"),
        (
            "sibling.crn",
            "let note = '😀' mock.test.Thing { name = 'one' }\nmock.test.Thing { name = 'two' }\n",
        ),
    ]);
    directory.assert_sibling_owner(
        &custom_engine(schemas),
        "identity is derived from mutable attributes",
        &[("mock.test.Thing", 0), ("mock.test.Thing", 1)],
    );
}

#[test]
fn closure_module_attribute_owns_inline_name_and_value() {
    for (source, message, token) in [
        (
            "let note = '😀'\nattributes { result: String = 1 }\n",
            "expected String",
            "result",
        ),
        (
            "let note = '😀'\nattributes { result = missing }\n",
            "Undefined resource 'missing'",
            "missing",
        ),
    ] {
        let directory = Directory::new(&[
            ("main.crn", "let label = 'main'\n"),
            ("sibling.crn", source),
        ]);
        directory.assert_sibling_owner(&test_engine_with_wait_target(), message, &[(token, 0)]);
    }
}
