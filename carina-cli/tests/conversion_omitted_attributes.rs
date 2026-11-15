#[path = "support/conversion_factory.rs"]
mod base;

use carina_core::{paths::ResolvedPath, provider::*, schema::*};
use std::{path::PathBuf, sync::Arc};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position};

const BARE_RESOURCE: &str =
    "'p' is a resource, not a value; reference one of its attributes, e.g. 'p.<attribute>'";

#[derive(Clone, Copy, Debug)]
enum Shape {
    Attribute,
    Map,
    List,
    Sibling,
}

impl Shape {
    fn value(self, converted: bool) -> String {
        let operand = if converted { "p as mock.Name" } else { "p" };
        match self {
            Self::Attribute => operand.to_string(),
            Self::Map | Self::Sibling => format!("{{ b = {operand} }}"),
            Self::List => format!("[{operand}]"),
        }
    }

    fn destination(self) -> AttributeType {
        let name = || {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Name")),
                None,
                Some((Some(1), Some(8))),
                None,
            )
        };
        match self {
            Self::Attribute => name(),
            Self::Map | Self::Sibling => AttributeType::struct_(
                "B",
                vec![
                    StructField::optional("b", name()).build(),
                    StructField::optional("ready", AttributeType::string()).build(),
                ],
            ),
            Self::List => AttributeType::list(name()),
        }
    }
}

struct OmittedFactory {
    inner: Box<dyn ProviderFactory>,
    shape: Shape,
}

impl ProviderFactory for OmittedFactory {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn display_name(&self) -> &str {
        self.inner.display_name()
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        inputs: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.inner.validate_config(attributes, inputs)
    }

    fn create_provider(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        self.inner.create_provider(session, mint)
    }

    fn create_normalizer(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        self.inner.create_normalizer(session, mint)
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let mut schemas = self.inner.schemas();
        schemas.push(
            ResourceSchema::new("OmittedSink")
                .attribute(AttributeSchema::optional("attr", self.shape.destination()).build())
                .validate()
                .unwrap(),
        );
        schemas
    }
}

fn factories(shape: Shape) -> Vec<Box<dyn ProviderFactory>> {
    base::factories()
        .into_iter()
        .map(|inner| Box::new(OmittedFactory { inner, shape }) as Box<dyn ProviderFactory>)
        .collect()
}

struct Fixture {
    directory: tempfile::TempDir,
    shape: Shape,
}

impl Fixture {
    fn new(shape: Shape) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            directory.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        Self { directory, shape }
    }

    fn document_path(&self) -> PathBuf {
        self.directory.path().join(match self.shape {
            Shape::Attribute | Shape::Map | Shape::List => "main.crn",
            Shape::Sibling => "b.crn",
        })
    }

    fn write(&self, converted: bool) -> String {
        let definition = "let p = mock.Source { output = 'ok' }\n";
        let sink = format!(
            "mock.OmittedSink {{ attr = {} }}\n",
            self.shape.value(converted)
        );
        let source = match self.shape {
            Shape::Attribute | Shape::Map | Shape::List => format!("{definition}{sink}"),
            Shape::Sibling => {
                std::fs::write(self.directory.path().join("a.crn"), definition).unwrap();
                sink
            }
        };
        std::fs::write(self.document_path(), &source).unwrap();
        source
    }

    fn validate(&self) -> serde_json::Value {
        carina_cli::commands::validate::validate_diagnostics_with_factories(
            &ResolvedPath::new(self.directory.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories(self.shape)),
            Default::default(),
        )
        .json
    }

    async fn plan(&self) -> Result<bool, Box<carina_cli::error::AppError>> {
        let runtime = carina_cli::test_support::TestCommandRuntime::new();
        carina_cli::commands::init::run_init(
            &runtime,
            self.directory.path(),
            false,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        carina_cli::test_support::run_plan_with_factories(
            &runtime,
            self.directory.path(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories(self.shape)),
        )
        .await
        .map_err(Box::new)
    }

    fn lsp(&self, source: String) -> Vec<Diagnostic> {
        self.lsp_at(self.document_path(), source)
    }

    fn lsp_at(&self, path: PathBuf, source: String) -> Vec<Diagnostic> {
        let loaded = loaded_provider_snapshot_from_factories(
            Arc::new(factories(self.shape)),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = carina_lsp::test_support::Diagnostics::new(
            self.directory.path(),
            Arc::new(loaded),
            vec!["mock".into()],
        );
        let document = carina_lsp::document::Document::new_with_path(
            source,
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        engine.analyze(&document, Some(&path))
    }
}

fn operand_position(source: &str, converted: bool) -> Position {
    let offset = if converted {
        source.find("p as mock.Name").unwrap()
    } else {
        source.rfind('p').unwrap()
    };
    let before = &source[..offset];
    Position::new(
        before.bytes().filter(|byte| *byte == b'\n').count() as u32,
        before.rsplit('\n').next().unwrap().len() as u32,
    )
}

fn error_at_operand(diagnostic: &Diagnostic, position: Position) -> bool {
    diagnostic.severity == Some(DiagnosticSeverity::ERROR)
        && diagnostic.range.start <= position
        && diagnostic.range.end >= Position::new(position.line, position.character + 1)
}

macro_rules! omitted_attribute_case {
    ($validate:ident, $plan:ident, $lsp:ident, $twin:ident, $shape:expr) => {
        #[test]
        fn $validate() {
            let fixture = Fixture::new($shape);
            let source = fixture.write(true);
            let json = fixture.validate();
            assert_eq!(json["status"], "error", "{source}\n{json}");
            let errors = json["errors"].as_array().unwrap();
            assert_eq!(
                errors
                    .iter()
                    .map(|finding| finding["message"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["expected mock.Name, got mock.Source", BARE_RESOURCE],
                "{json}",
            );
            assert!(
                errors.iter().all(|finding| finding["code"].is_null()),
                "{json}"
            );
        }

        #[tokio::test]
        async fn $plan() {
            let fixture = Fixture::new($shape);
            let source = fixture.write(true);
            let error = fixture
                .plan()
                .await
                .expect_err("an omitted resource operand must fail validation");
            let rendered = carina_cli::error::format_error_with_source_chain(&error);
            assert!(rendered.contains(BARE_RESOURCE), "{source}\n{rendered}");
            assert!(
                !rendered.contains("conversion cannot be checked here"),
                "{rendered}"
            );
        }

        #[test]
        fn $lsp() {
            let fixture = Fixture::new($shape);
            let source = fixture.write(true);
            let position = operand_position(&source, true);
            let diagnostics = fixture.lsp(source.clone());
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| error_at_operand(diagnostic, position)),
                "{source}\n{diagnostics:#?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.message.contains("conversion")),
                "{diagnostics:?}"
            );
        }

        #[tokio::test]
        async fn $twin() {
            let fixture = Fixture::new($shape);
            let source = fixture.write(false);
            let json = fixture.validate();
            assert_eq!(json["status"], "error", "{source}\n{json}");
            let errors = json["errors"].as_array().unwrap();
            assert_eq!(
                errors
                    .iter()
                    .map(|finding| finding["message"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["expected mock.Name, got mock.Source", BARE_RESOURCE],
                "{json}",
            );
            assert!(
                errors.iter().all(|finding| finding["code"].is_null()),
                "{json}"
            );
            let position = operand_position(&source, false);
            let diagnostics = fixture.lsp(source.clone());
            assert!(
                diagnostics.iter().any(|diagnostic| {
                    error_at_operand(diagnostic, position) && diagnostic.message == BARE_RESOURCE
                }),
                "{source}\n{diagnostics:#?}"
            );
            let error = fixture.plan().await.unwrap_err();
            let output = carina_cli::error::format_error_with_source_chain(&error);
            assert!(output.contains(BARE_RESOURCE), "{source}\n{output}");
        }
    };
}

omitted_attribute_case!(
    r8_omitted_attribute_fails_validate,
    r8_omitted_attribute_cannot_plan,
    r8_omitted_attribute_has_an_lsp_error,
    r8_no_as_attribute_keeps_mains_error,
    Shape::Attribute
);
omitted_attribute_case!(
    r8_omitted_map_fails_validate,
    r8_omitted_map_cannot_plan,
    r8_omitted_map_has_an_lsp_error,
    r8_no_as_map_keeps_mains_error,
    Shape::Map
);
omitted_attribute_case!(
    r8_omitted_list_fails_validate,
    r8_omitted_list_cannot_plan,
    r8_omitted_list_has_an_lsp_error,
    r8_no_as_list_keeps_mains_error,
    Shape::List
);
omitted_attribute_case!(
    r8_omitted_sibling_binding_fails_validate,
    r8_omitted_sibling_binding_cannot_plan,
    r8_omitted_sibling_binding_has_an_lsp_error,
    r8_no_as_sibling_binding_keeps_mains_error,
    Shape::Sibling
);

#[tokio::test]
async fn r9_converted_alias_keeps_the_ordinary_bare_resource_gate() {
    let fixture = Fixture::new(Shape::Attribute);
    let source = "let p = mock.Source { output = 'ok' }\nlet q = p as mock.Name\nmock.OmittedSink { attr = q }\n";
    std::fs::write(fixture.document_path(), source).unwrap();
    let json = fixture.validate();
    assert_eq!(json["status"], "error", "{json}");
    let errors = json["errors"].as_array().unwrap();
    assert!(errors.iter().any(|error| error["message"] == "'q' is a resource, not a value; reference one of its attributes, e.g. 'q.<attribute>'"), "{json}");
    assert!(errors.iter().all(|error| error["code"].is_null()), "{json}");
    let error = fixture
        .plan()
        .await
        .expect_err("the converted alias is still a bare resource");
    let rendered = carina_cli::error::format_error_with_source_chain(&error);
    assert!(
        rendered.contains("'q' is a resource, not a value"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("conversion cannot be checked here"),
        "{rendered}"
    );
    let diagnostics = fixture.lsp(source.into());
    assert!(
        diagnostics.iter().any(
            |diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)
                && diagnostic
                    .message
                    .contains("'q' is a resource, not a value")
        ),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("conversion")),
        "{diagnostics:?}"
    );
}

#[test]
fn r9_live_omission_routes_fail_validate_and_lsp() {
    let cases = [
        (
            "scalar",
            Shape::Attribute,
            "mock.OmittedSink { attr = p as mock.Name }",
        ),
        (
            "list",
            Shape::List,
            "mock.OmittedSink { attr = [p as mock.Name] }",
        ),
        (
            "partial_list",
            Shape::List,
            "mock.OmittedSink { attr = ['ok', p as mock.Name] }",
        ),
        (
            "map",
            Shape::Map,
            "mock.OmittedSink { attr = { b = p as mock.Name } }",
        ),
        (
            "partial_map",
            Shape::Map,
            "mock.OmittedSink { attr = { ready = 'ok' b = p as mock.Name } }",
        ),
        (
            "nested_block",
            Shape::Attribute,
            "mock.Sink { entry { value = p as mock.Name } }",
        ),
        (
            "repeated_blocks",
            Shape::Attribute,
            "mock.Sink { entry { value = 'ok' } entry { value = p as mock.Name } }",
        ),
        (
            "map_block",
            Shape::Map,
            "mock.OmittedSink { attr = { entry { value = p as mock.Name } } }",
        ),
        (
            "repeated_key",
            Shape::Attribute,
            "mock.OmittedSink { attr = p as mock.Name attr = 'ok' }",
        ),
        (
            "reversed_key",
            Shape::Attribute,
            "mock.OmittedSink { attr = 'ok' attr = p as mock.Name }",
        ),
        (
            "repeated_map_key",
            Shape::Map,
            "mock.OmittedSink { attr = { b = p as mock.Name b = 'ok' } }",
        ),
        (
            "alias",
            Shape::Attribute,
            "let q = p as mock.Name\nmock.OmittedSink { attr = q }",
        ),
        (
            "sibling",
            Shape::Sibling,
            "mock.OmittedSink { attr = { b = p as mock.Name } }",
        ),
        (
            "selected_if",
            Shape::Attribute,
            "mock.OmittedSink { attr = if true { p as mock.Name } else { 'ok' as mock.Name } }",
        ),
        (
            "coalesce_left",
            Shape::Attribute,
            "mock.OmittedSink { attr = (p as mock.Name) ?? ('ok' as mock.Name) }",
        ),
        (
            "coalesce_right",
            Shape::Attribute,
            "mock.OmittedSink { attr = ('ok' as mock.Name) ?? (p as mock.Name) }",
        ),
        (
            "coalesce_repeated",
            Shape::Attribute,
            "mock.OmittedSink { attr = (p as mock.Name) ?? (p as mock.Name) }",
        ),
        (
            "parens",
            Shape::Attribute,
            "mock.OmittedSink { attr = (p as mock.Name) }",
        ),
        (
            "read",
            Shape::Attribute,
            "let q = read mock.Query { attr = p as mock.Name }",
        ),
        (
            "deferred_for",
            Shape::Attribute,
            "for n in p.items { mock.OmittedSink { attr = p as mock.Name } }",
        ),
    ];
    for (label, shape, tail) in cases {
        for converted in [false, true] {
            let fixture = Fixture::new(shape);
            let definition = "let p = mock.Source { output = 'ok' }\n";
            let source = if matches!(shape, Shape::Sibling) {
                std::fs::write(fixture.directory.path().join("a.crn"), definition).unwrap();
                format!("{tail}\n")
            } else {
                format!("{definition}{tail}\n")
            };
            let source = if converted {
                source
            } else {
                source.replace(" as mock.Name", "")
            };
            std::fs::write(fixture.document_path(), &source).unwrap();
            let json = fixture.validate();
            assert_eq!(
                json["status"], "error",
                "{label}, converted={converted}: {json}"
            );
            assert!(
                !json.to_string().contains("bug in carina"),
                "{label}: {json}"
            );
            let diagnostics = fixture.lsp(source);
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
                "{label}, converted={converted}: {diagnostics:?}"
            );
            if !converted {
                let expected = match label {
                    "repeated_blocks" | "map_block" | "repeated_map_key" | "partial_list" => {
                        vec![BARE_RESOURCE]
                    }
                    "alias" => vec![
                        "expected mock.Name, got mock.Source",
                        "'q' is a resource, not a value; reference one of its attributes, e.g. 'q.<attribute>'",
                    ],
                    "read" | "deferred_for" => vec![
                        "expected mock.Name, got mock.Source",
                        "expected mock.Name, got mock.Source",
                        BARE_RESOURCE,
                    ],
                    "coalesce_left" => vec![
                        "coalesce default type `String` does not match left type `mock.Source`",
                        BARE_RESOURCE,
                    ],
                    "coalesce_right" => vec![
                        "coalesce default type `mock.Source` does not match left type `String`",
                        BARE_RESOURCE,
                    ],
                    "coalesce_repeated" => vec![
                        "expected mock.Name, got mock.Source",
                        BARE_RESOURCE,
                        BARE_RESOURCE,
                    ],
                    "scalar" | "list" | "map" | "partial_map" | "nested_block" | "repeated_key"
                    | "reversed_key" | "sibling" | "selected_if" | "parens" => {
                        vec!["expected mock.Name, got mock.Source", BARE_RESOURCE]
                    }
                    unexpected => panic!("missing baseline for {unexpected}"),
                };
                assert_eq!(
                    json["errors"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|finding| finding["message"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    expected,
                    "{label}: {json}"
                );
                assert!(
                    json["errors"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|finding| finding["code"].is_null()),
                    "{label}: {json}"
                );
            }
        }
    }
}

#[test]
fn r9_converted_module_map_argument_keeps_the_pending_record() {
    let fixture = Fixture::new(Shape::Map);
    std::fs::create_dir(fixture.directory.path().join("child")).unwrap();
    std::fs::write(
        fixture.directory.path().join("child/main.crn"),
        "arguments { input: map(String) }\nattributes { out: String = 'ok' }\n",
    )
    .unwrap();
    let source = "let p = mock.Source { output = 'ok' }\nlet child = use { source = './child' }\nlet net = child { input = { b = p as mock.Name } }\n";
    std::fs::write(fixture.document_path(), source).unwrap();
    let json = fixture.validate();
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{json}");
    assert!(
        errors
            .iter()
            .any(|finding| finding["code"] == "conversion_unsupported_position"),
        "{json}"
    );
    assert!(
        errors
            .iter()
            .any(|finding| finding["message"].as_str().unwrap().contains(BARE_RESOURCE)),
        "{json}"
    );
    assert!(
        !json.to_string().contains("Missing required argument"),
        "{json}"
    );
    let diagnostics = fixture.lsp(source.into());
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains(BARE_RESOURCE)),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .contains("conversion is not supported at this position yet")),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("Missing required argument")),
        "{diagnostics:?}"
    );
    let twin = source.replace(" as mock.Name", "");
    std::fs::write(fixture.document_path(), &twin).unwrap();
    let json = fixture.validate();
    assert_eq!(json["errors"].as_array().unwrap().len(), 1, "{json}");
    assert!(
        json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains(BARE_RESOURCE),
        "{json}"
    );
    assert!(json["errors"][0]["code"].is_null(), "{json}");
}

#[tokio::test]
async fn r9_json_plan_cannot_save_a_resource_with_an_omitted_attribute() {
    for shape in [Shape::Attribute, Shape::List, Shape::Map, Shape::Sibling] {
        let fixture = Fixture::new(shape);
        fixture.write(true);
        let runtime = carina_cli::test_support::TestCommandRuntime::new();
        carina_cli::commands::init::run_init(
            &runtime,
            fixture.directory.path(),
            false,
            false,
            false,
            false,
        )
        .await
        .unwrap();
        let output = fixture.directory.path().join("rejected-plan.json");
        let result = carina_cli::test_support::run_saved_json_plan_with_factories(
            &runtime,
            fixture.directory.path(),
            &output,
            carina_cli::wiring::LoadedProviderFactory::for_test(factories(shape)),
        )
        .await;
        let error = result.expect_err("validation must fail before constructing or saving a plan");
        assert!(
            carina_cli::error::format_error_with_source_chain(&error).contains(BARE_RESOURCE),
            "{shape:?}: {error:?}"
        );
        assert!(
            !output.exists(),
            "{shape:?}: a rejected configuration wrote a plan"
        );
    }
}

#[test]
fn r9_cross_file_converted_structural_alias_has_only_the_twins_ordinary_errors() {
    let fixture = Fixture::new(Shape::Sibling);
    let a = "let p = mock.Source { output = 'ok' }\nlet q = p as mock.Name\n";
    let b = "mock.OmittedSink { attr = { b = q } }\n";
    std::fs::write(fixture.directory.path().join("a.crn"), a).unwrap();
    std::fs::write(fixture.document_path(), b).unwrap();
    let json = fixture.validate();
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 2, "{json}");
    assert_eq!(
        errors[0]["message"], "expected mock.Name, got mock.Source",
        "{json}"
    );
    assert!(
        errors[1]["message"]
            .as_str()
            .unwrap()
            .contains("'q' is a resource, not a value"),
        "{json}"
    );
    assert!(
        errors.iter().all(|finding| finding["code"].is_null()),
        "{json}"
    );
    for (file, source) in [("a.crn", a), ("b.crn", b)] {
        let diagnostics = fixture.lsp_at(fixture.directory.path().join(file), source.into());
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("conversion")
                    && !diagnostic.message.contains("bug in carina")),
            "{file}: {diagnostics:?}"
        );
        if file == "b.crn" {
            assert!(
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
                "{diagnostics:?}"
            );
        }
    }
}

#[test]
fn r9_non_live_bodies_and_module_instance_keep_their_ordinary_gates() {
    for scene in [
        "empty_for",
        "false_if",
        "live_module_if",
        "false_module_if",
        "unused_module",
    ] {
        for converted in [false, true] {
            let fixture = Fixture::new(Shape::Attribute);
            let tail = match scene {
                "empty_for" => "for n in [] { mock.OmittedSink { attr = p as mock.Name } }",
                "false_if" => "if false { mock.OmittedSink { attr = p as mock.Name } }",
                "live_module_if" => {
                    "let child = use { source = './child' }\nlet net = child { enabled = true }"
                }
                "false_module_if" => {
                    "let child = use { source = './child' }\nlet net = child { enabled = false }"
                }
                "unused_module" => "let child = use { source = './child' }",
                unexpected => panic!("unknown scene {unexpected}"),
            };
            if scene.ends_with("module_if") || scene == "unused_module" {
                std::fs::create_dir(fixture.directory.path().join("child")).unwrap();
                let child = "arguments { enabled: Bool }\nlet p = mock.Source { output = 'ok' }\nif enabled { mock.OmittedSink { attr = p as mock.Name } }\n";
                std::fs::write(
                    fixture.directory.path().join("child/main.crn"),
                    if converted {
                        child.into()
                    } else {
                        child.replace(" as mock.Name", "")
                    },
                )
                .unwrap();
            }
            let source = format!("let p = mock.Source {{ output = 'ok' }}\n{tail}\n");
            let source = if converted {
                source
            } else {
                source.replace(" as mock.Name", "")
            };
            std::fs::write(fixture.document_path(), &source).unwrap();
            let json = fixture.validate();
            println!("NONLIVE {scene} converted={converted}: {json}");
            assert!(
                json["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .all(|finding| finding["code"].is_null()),
                "{scene}: {json}"
            );
            let diagnostics = fixture.lsp(source);
            println!("NONLIVE-LSP {scene} converted={converted}: {diagnostics:?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| !diagnostic.message.contains("conversion")
                        && !diagnostic.message.contains("bug in carina")),
                "{scene}: {diagnostics:?}"
            );
            if scene == "live_module_if" {
                assert_eq!(json["status"], "error", "{json}");
                assert!(
                    diagnostics
                        .iter()
                        .any(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR)),
                    "{diagnostics:?}"
                );
            }
        }
    }
}

#[test]
fn r9_audit_directive_storage_and_unsupported_keys() {
    for (label, source) in [
        (
            "unsupported_directive",
            "mock.example.thing { directives { unsupported_key = 'x' } }",
        ),
        (
            "depends_on",
            "let p = mock.example.thing {}\nmock.example.thing { directives { depends_on = [p] } }",
        ),
    ] {
        let fixture = Fixture::new(Shape::Attribute);
        std::fs::write(fixture.document_path(), source).unwrap();
        let catalog =
            carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let parsed =
            carina_core::parser::parse_file(source, &fixture.document_path(), &catalog).unwrap();
        for resource in parsed.iter_all_resources() {
            println!(
                "DIRECTIVES {label}: values={:?}, directives={:?}",
                resource.attributes(),
                resource.directives()
            );
        }
        let parsed = parsed.resolve_root_for_test();
        let schemas = SchemaRegistry::new();
        let configuration =
            carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let tree = carina_core::parser::AnalysisTree::try_from_file(configuration.judged().file())
            .unwrap();
        for resource in tree.resources() {
            if let Ok(slot) = resource.authored_slot("directives") {
                println!(
                    "DIRECTIVE-SLOT {label}: value={}, conversion_free={:?}",
                    slot.projection().is_some(),
                    slot.cursor()
                        .map(|cursor| cursor.node().judgement().no_conversion().is_some())
                );
            }
        }
        let json = fixture.validate();
        println!("DIRECTIVES {label}: {json}");
        assert!(
            json["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|finding| finding["code"].is_null()),
            "{json}"
        );
    }
}
