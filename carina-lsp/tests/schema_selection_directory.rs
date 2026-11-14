use std::sync::Arc;

use carina_core::parser::{ProviderContext, TypeExpr};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
};
use carina_lsp::{completion::CompletionProvider, document::Document, hover::HoverProvider};
use tower_lsp::lsp_types::{CompletionItem, CompletionTextEdit, HoverContents, Position};

struct Fixture {
    directory: tempfile::TempDir,
    schemas: Arc<SchemaRegistry>,
}

impl Fixture {
    fn new(main: &str, exports: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("main.crn"), main).unwrap();
        std::fs::write(directory.path().join("exports.crn"), exports).unwrap();
        std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        let mut schemas = SchemaRegistry::new();
        for (read, attribute, description) in [
            (false, "managed_only", "Managed VPC"),
            (true, "lookup_only", "Existing VPC"),
        ] {
            let options = AttributeType::struct_(
                "Options",
                vec![
                    StructField::optional(attribute, AttributeType::string()).build(),
                    StructField::optional(
                        "mode",
                        if read {
                            AttributeType::bool()
                        } else {
                            AttributeType::string()
                        },
                    )
                    .with_description(description)
                    .build(),
                ],
            );
            let schema = ResourceSchema::new("ec2.Vpc")
                .with_description(description)
                .attribute(AttributeSchema::optional(attribute, AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("shared", AttributeType::string())
                        .with_description(description)
                        .build(),
                )
                .attribute(AttributeSchema::optional("options", options).build());
            schemas.insert(
                "mock",
                if read {
                    schema.as_data_source()
                } else {
                    schema
                }
                .validate()
                .unwrap(),
            );
        }
        schemas.insert(
            "mock",
            ResourceSchema::new("ec2.Subnet")
                .attribute(
                    AttributeSchema::optional("subnet_only", AttributeType::string()).build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("compute.Image")
                .as_data_source()
                .with_description("Existing image")
                .attribute(AttributeSchema::required("filter", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("id", AttributeType::string())
                        .with_description("Image identity")
                        .build(),
                )
                .validate()
                .unwrap(),
        );
        Self {
            directory,
            schemas: Arc::new(schemas),
        }
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn document(&self, name: &str, marked: &str, loaded_types: bool) -> (Document, Position) {
        let cursor = marked.find('|').expect("cursor marker");
        let text = marked.replacen('|', "", 1);
        let position = carina_lsp::position::byte_offset_to_position(&text, cursor).unwrap();
        let mut context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        if loaded_types {
            context.resource_types =
                ProviderContext::resource_types_from_schema_registry(&self.schemas);
        }
        let document =
            Document::new_with_path(text, self.directory.path().join(name), Arc::new(context));
        (document, position)
    }

    fn complete(&self, name: &str, marked: &str) -> Vec<CompletionItem> {
        let (document, position) = self.document(name, marked, true);
        let path = self.directory.path().join(name);
        CompletionProvider::new(
            Arc::clone(&self.schemas),
            vec!["mock".into()],
            vec![],
            vec![],
        )
        .complete(
            &document,
            position,
            path.parent(),
            document.provider_context(),
        )
    }

    fn hover(&self, name: &str, marked: &str, loaded_types: bool) -> Option<String> {
        let (document, position) = self.document(name, marked, loaded_types);
        let path = self.directory.path().join(name);
        let file_name = path.file_name().unwrap().to_str().unwrap();
        HoverProvider::new(Arc::clone(&self.schemas), vec![])
            .hover_with_context(&document, position, path.parent(), Some(file_name))
            .map(|hover| match hover.contents {
                HoverContents::Markup(content) => content.value,
                other => panic!("expected markup, got {other:?}"),
            })
    }
}

fn labels(items: &[CompletionItem]) -> Vec<&str> {
    items.iter().map(|item| item.label.as_str()).collect()
}

fn module_fixture() -> Fixture {
    let fixture = Fixture::new(
        "let local_module = use { source = './local' }\nlet local = local_module {}\n",
        "exports {\n  image_id: String = local.r.id\n}\n",
    );
    fixture.write(
        "local/resources.crn",
        "let img = read mock.compute.Image { filter = 'linux' }\n",
    );
    fixture.write("local/attributes.crn", "attributes { r = img }\n");
    fixture.write("local/providers.crn", "provider mock {}\n");
    fixture
}

#[test]
fn data_source_only_module_handle_completion() {
    let fixture = module_fixture();
    let items = fixture.complete(
        "exports.crn",
        "exports {\n  image_id: String = local.r.|\n}\n",
    );
    let labels = labels(&items);
    assert!(labels.contains(&"local.r.id"), "{labels:?}");
    assert!(labels.contains(&"local.r.filter"), "{labels:?}");
}

#[test]
fn unregistered_module_output_type_falls_through_to_later_completers() {
    let fixture = Fixture::new(
        "let local_module = use { source = './local' }\nlet m = local_module {}\n",
        "exports {}\n",
    );
    fixture.write("local/values.crn", "let value = 'non-handle'\n");
    fixture.write(
        "local/attributes.crn",
        "attributes { out: foo.bar.Baz = value }\n",
    );
    let items = fixture.complete("exports.crn", "let x = m.out.|\n");
    let labels = labels(&items);
    assert!(labels.contains(&"let"), "{labels:?}");
    assert!(labels.contains(&"mock.ec2.Vpc"), "{labels:?}");
}

#[test]
fn data_source_only_module_origin_hover() {
    let fixture = module_fixture();
    let hover = fixture
        .hover(
            "local/resources.crn",
            "let img = read mock.compute.Im|age { filter = 'linux' }",
            true,
        )
        .unwrap();
    assert!(hover.contains("Existing image"), "{hover}");
    // origin/main does not offer hover on a postfix module handle itself.
    assert!(
        fixture
            .hover(
                "exports.crn",
                "exports {\n  image_id: String = local.r.|\n}\n",
                true
            )
            .is_none()
    );
}

#[test]
fn data_source_only_annotation_hover() {
    let source = "arguments {\n  x: mock.compute.Image\n}\n";
    let fixture = Fixture::new("", source);
    for loaded_types in [false, true] {
        let marked = source.replace("Image", "Im|age");
        let (document, _) = fixture.document("exports.crn", &marked, loaded_types);
        let annotation = &document.parsed().unwrap().arguments[0].static_type;
        // Parsing keeps dotted annotations unresolved even with loaded types;
        // core resolves them at the validation/inference boundary.
        assert!(matches!(annotation, TypeExpr::DottedUnresolved(_)));
        let hover = fixture
            .hover("exports.crn", &marked, loaded_types)
            .expect("kindless annotation resolves data-source-only schema");
        assert!(hover.contains("Existing image"), "{hover}");
    }
}

#[test]
fn multiline_nested_annotation_hover() {
    let source = "arguments {\n  x:\n    list(\n      mock.compute.Image\n    )\n}\n";
    let fixture = Fixture::new("", source);
    for loaded_types in [false, true] {
        let marked = source.replace("Image", "Im|age");
        let (document, _) = fixture.document("exports.crn", &marked, loaded_types);
        assert!(document.parsed().is_some(), "annotation fixture must parse");
        let hover = fixture
            .hover("exports.crn", &marked, loaded_types)
            .expect("nested annotation has no syntax kind");
        assert!(hover.contains("Existing image"), "{hover}");
    }
}

#[test]
fn kindless_annotation_prefers_managed_schema() {
    let fixture = Fixture::new("", "arguments {\n  x: mock.ec2.Vpc\n}\n");
    let hover = fixture
        .hover("exports.crn", "arguments {\n  x: mock.ec2.V|pc\n}\n", true)
        .unwrap();
    assert!(hover.contains("Managed VPC"), "{hover}");
    assert!(!hover.contains("Existing VPC"), "{hover}");
}

#[test]
fn kindless_module_handle_prefers_managed_schema() {
    let fixture = module_fixture();
    fixture.write("local/resources.crn", "let img = read mock.ec2.Vpc {}\n");
    let items = fixture.complete("exports.crn", "exports {\n  value: String = local.r.|\n}\n");
    let labels = labels(&items);
    assert!(labels.contains(&"local.r.managed_only"), "{labels:?}");
    assert!(!labels.contains(&"local.r.lookup_only"), "{labels:?}");
}

#[test]
fn read_and_managed_type_and_attribute_hover() {
    let fixture = Fixture::new(
        "let managed = mock.ec2.Vpc {}\nlet lookup = read mock.ec2.Vpc {}\n",
        "exports {}\n",
    );
    for (read, description, other) in [
        ("", "Managed VPC", "Existing VPC"),
        ("read ", "Existing VPC", "Managed VPC"),
    ] {
        for marked in [
            format!("let v = {read}mock.ec2.V|pc {{}}\n"),
            format!("let v = {read}mock.ec2.Vpc {{\n  sha|red = 'x'\n}}\n"),
        ] {
            let hover = fixture.hover("main.crn", &marked, true).unwrap();
            assert!(hover.contains(description), "{hover}");
            assert!(!hover.contains(other), "{hover}");
        }
    }
}

#[test]
fn read_and_managed_in_block_completion() {
    let fixture = Fixture::new("", "exports {}\n");
    for (read, expected, other) in [
        ("", "managed_only", "lookup_only"),
        ("read ", "lookup_only", "managed_only"),
    ] {
        let items = fixture.complete(
            "main.crn",
            &format!("let v = {read}mock.ec2.Vpc {{\n  |\n}}\n"),
        );
        let labels = labels(&items);
        assert!(labels.contains(&expected), "{labels:?}");
        assert!(!labels.contains(&other), "{labels:?}");
    }
}

#[test]
fn multiline_read_binding_dot_in_sibling_file() {
    for header in [
        "read mock.ec2.Vpc",
        "read\n  mock.ec2.Vpc",
        "read // lookup\n  /* trivia */\n  mock.ec2.Vpc",
        "\n  read\n  mock.ec2.Vpc",
    ] {
        let fixture = Fixture::new(
            &format!("let x = {header} {{}}\n"),
            "attributes {\n  value = x.shared\n}\n",
        );
        let items = fixture.complete("exports.crn", "attributes {\n  value = x.|\n}\n");
        let labels = labels(&items);
        assert!(labels.contains(&"x.lookup_only"), "{header}: {labels:?}");
        assert!(!labels.contains(&"x.managed_only"), "{header}: {labels:?}");
    }
}

#[test]
fn multiline_managed_binding_dot_in_sibling_file() {
    for header in ["mock.ec2.Vpc", "\n  mock.ec2.Vpc"] {
        let fixture = Fixture::new(
            &format!("let x = {header} {{}}\n"),
            "attributes {\n  value = x.shared\n}\n",
        );
        let items = fixture.complete("exports.crn", "attributes {\n  value = x.|\n}\n");
        let labels = labels(&items);
        assert!(labels.contains(&"x.managed_only"), "{header}: {labels:?}");
        assert!(!labels.contains(&"x.lookup_only"), "{header}: {labels:?}");
    }
}

#[test]
fn read_kind_inside_struct_completion() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete(
        "main.crn",
        "let x = read mock.ec2.Vpc {\n  options {\n    |\n  }\n}\n",
    );
    let labels = labels(&items);
    assert!(labels.contains(&"lookup_only"), "{labels:?}");
    assert!(!labels.contains(&"managed_only"), "{labels:?}");
}

#[test]
fn read_kind_after_equals_in_struct_completion() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete(
        "main.crn",
        "let x = read mock.ec2.Vpc {\n  options {\n    mode = |\n  }\n}\n",
    );
    let labels = labels(&items);
    assert!(
        labels.contains(&"true") && labels.contains(&"false"),
        "{labels:?}"
    );
}

#[test]
fn read_kind_in_ignore_changes_completion() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete(
        "main.crn",
        "let x = read mock.ec2.Vpc {\n  directives {\n    ignore_changes = [|]\n  }\n}\n",
    );
    let labels = labels(&items);
    assert!(labels.contains(&"lookup_only"), "{labels:?}");
    assert!(labels.contains(&"options.lookup_only"), "{labels:?}");
    assert!(!labels.contains(&"managed_only"), "{labels:?}");
    assert!(!labels.contains(&"options.managed_only"), "{labels:?}");
}

#[test]
fn walk_back_annotation_is_not_resource_header() {
    let fixture = Fixture::new("", "exports {}\n");
    let source = "arguments {\n  sg:\n    mock.ec2.Vpc {\n      shared\n    }\n}\nlet v = mock.ec2.Vpc {\n  shared = 'x'\n}\n";
    let marked = source.replacen("shared", "sha|red", 1);
    assert!(fixture.hover("main.crn", &marked, true).is_none());
    let items = fixture.complete("main.crn", &marked);
    assert!(!labels(&items).contains(&"managed_only"), "{items:?}");
    let marked = source.replace("shared = 'x'", "sha|red = 'x'");
    let hover = fixture.hover("main.crn", &marked, true).unwrap();
    assert!(hover.contains("Managed VPC"), "{hover}");
    let items = fixture.complete("main.crn", &marked);
    assert!(labels(&items).contains(&"managed_only"), "{items:?}");
}

#[test]
fn walk_back_assignment_is_not_resource_header() {
    let fixture = Fixture::new("", "exports {}\n");
    let source =
        "attr =\n  mock.ec2.Vpc {\n    shared\n  }\nlet v = mock.ec2.Vpc {\n  shared = 'x'\n}\n";
    let marked = source.replacen("shared", "sha|red", 1);
    assert!(fixture.hover("main.crn", &marked, true).is_none());
    let items = fixture.complete("main.crn", &marked);
    assert!(!labels(&items).contains(&"managed_only"), "{items:?}");
    let marked = source.replace("shared = 'x'", "sha|red = 'x'");
    let hover = fixture.hover("main.crn", &marked, true).unwrap();
    assert!(hover.contains("Managed VPC"), "{hover}");
    let items = fixture.complete("main.crn", &marked);
    assert!(labels(&items).contains(&"managed_only"), "{items:?}");
}

#[test]
fn walk_back_let_preserves_current_binding() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete(
        "main.crn",
        "let x =\n  mock.ec2.Vpc {\n    shared = |\n  }\n",
    );
    let labels = labels(&items);
    for self_reference in ["x", "x.shared", "x.managed_only"] {
        assert!(!labels.contains(&self_reference), "{labels:?}");
    }
}

fn assert_sibling_selector(rhs: &str, attribute: Option<&str>) {
    for condition in [false, true] {
        let source = format!(
            "let c = {condition}\nlet d = false\nlet y = 'a'\nlet z = 'b'\n\
             let settings = {{ enabled = true }}\n\
             fn check(value: String): Bool {{ true }}\nlet x = {rhs}\n"
        );
        carina_core::parser::parse_syntax_inventory(&source, std::path::Path::new("main.crn"))
            .unwrap_or_else(|error| panic!("fixture must be grammar-valid: {source}: {error:?}"));
        assert_sibling_selector_source(&source, attribute);
    }
}

fn assert_sibling_selector_source(source: &str, attribute: Option<&str>) {
    let fixture = Fixture::new(source, "exports {}\n");
    if attribute.is_some() {
        // Grammar acceptance alone also admits structural expressions that
        // core cannot lower. Every positive selector fixture must lower too.
        let mut context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        context.resource_types =
            ProviderContext::resource_types_from_schema_registry(&fixture.schemas);
        carina_core::parser::parse_file(
            source,
            &fixture.directory.path().join("main.crn"),
            &context,
        )
        .unwrap_or_else(|error| {
            panic!("positive fixture must lower through core: {source}: {error}")
        });
    }
    let items = fixture.complete("exports.crn", "attributes {\n  value = x.|\n}\n");
    let labels = labels(&items);
    for candidate in ["managed_only", "lookup_only", "subnet_only", "filter", "id"] {
        assert_eq!(
            labels.contains(&format!("x.{candidate}").as_str()),
            attribute == Some(candidate),
            "{source}: {labels:?}"
        );
    }
}

#[test]
fn direct_resource_bindings_also_lower_through_core() {
    assert_sibling_selector("mock.ec2.Vpc {}", Some("managed_only"));
    assert_sibling_selector("read mock.ec2.Vpc {}", Some("lookup_only"));
}

#[test]
fn module_call_bindings_have_no_resource_selector() {
    for rhs in [
        "mod {}",
        "if c { mod {} }",
        "if c { mod {} } else { mod {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn conditional_binding_same_type_single_line() {
    assert_sibling_selector(
        "if c { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        Some("managed_only"),
    );
}

#[test]
fn conditional_binding_same_type_multiline() {
    for rhs in [
        "if c {\n  mock.ec2.Vpc {}\n} else {\n  mock.ec2.Vpc {}\n}",
        "\n  if c {\n    mock.ec2.Vpc {}\n  }\n  else {\n    mock.ec2.Vpc {}\n  }",
        "if c {\r\n  mock.ec2.Vpc {}\r\n} else {\r\n  mock.ec2.Vpc {}\r\n}",
        "if c {\n  mock.ec2.Vpc\n  {}\n} else {\n  mock.ec2.Vpc\n  {}\n}",
    ] {
        assert_sibling_selector(rhs, Some("managed_only"));
    }
}

#[test]
fn conditional_binding_uses_branch_results_not_condition_or_locals() {
    for rhs in [
        "if settings.enabled { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if check('literal { else }') { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if (c) { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if c { let local = 1\n mock.ec2.Vpc {} } else { let local = 'a'\n mock.ec2.Vpc {} }",
        "if c { let local = y ?? z\n mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if c { let local = if d { 1 } else { 2 }\n mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, Some("managed_only"));
    }
}

#[test]
fn conditional_binding_declarations_in_value_positions_are_untyped() {
    for rhs in [
        "if mock.ec2.Subnet {} { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if c { let local = mock.ec2.Subnet {} mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if c { let local = mock.ec2.Subnet {} ?? mock.ec2.Vpc {} mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn conditional_binding_same_read_type() {
    assert_sibling_selector(
        "if c { read mock.ec2.Vpc {} } else { read mock.ec2.Vpc {} }",
        Some("lookup_only"),
    );
}

#[test]
fn conditional_binding_different_types_is_untyped() {
    assert_sibling_selector("if c { mock.ec2.Vpc {} } else { mock.ec2.Subnet {} }", None);
}

#[test]
fn conditional_binding_different_kinds_is_untyped() {
    for rhs in [
        "if c { read mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }",
        "if c { mock.ec2.Vpc {} } else { read mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn conditional_binding_non_resource_branch_is_untyped() {
    for rhs in [
        "if c { mock.ec2.Vpc {} } else { y }",
        "if c { y } else { mock.ec2.Vpc {} }",
        "if c { mock.ec2.Vpc {} } else { 'mock.ec2.Vpc' }",
        "if c { mock.ec2.Vpc {} } else { mod {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn conditional_binding_wrapped_resource_tails_are_untyped() {
    for rhs in [
        "if c { (mock.ec2.Vpc {}) } else { mock.ec2.Vpc {} }",
        "if c { mock.ec2.Vpc {} } else { (mock.ec2.Vpc {}) }",
        "if c { (read mock.ec2.Vpc {}) } else { (read mock.ec2.Vpc {}) }",
        "if c { mock.ec2.Vpc {} ?? y } else { mock.ec2.Vpc {} }",
        "if c { mock.ec2.Vpc {} } else { y ?? mock.ec2.Vpc {} }",
        "if c { mock.ec2.Vpc {} ?? mock.ec2.Vpc {} } else { mock.ec2.Vpc {} ?? mock.ec2.Vpc {} }",
    ] {
        // A direct resource before `??` also fails core's branch grammar.
        for condition in [false, true] {
            let source = format!("let c = {condition}\nlet y = 'a'\nlet x = {rhs}\n");
            assert_sibling_selector_source(&source, None);
        }
    }
}

#[test]
fn conditional_binding_missing_branch_is_untyped() {
    for source in [
        "let x = if c { mock.ec2.Vpc {} } else {\n",
        "let x = if c { mock.ec2.Vpc {} } else {}\n",
        "let x = if c { mock.ec2.Vpc {}\n",
        "let x = if c { mock.ec2.Vpc {} } else\n",
    ] {
        assert_sibling_selector_source(source, None);
    }
}

#[test]
fn conditional_binding_without_else_uses_single_branch() {
    for rhs in [
        "if c { mock.ec2.Vpc {} }",
        "if c {\n  mock.ec2.Vpc {}\n}",
        "if c { let l = 1\n mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, Some("managed_only"));
    }
}

#[test]
fn conditional_binding_nested_if_is_untyped() {
    for rhs in [
        "if c { mock.ec2.Vpc {} } else { if d { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} } }",
        "if c { mock.ec2.Vpc {} } else { if d { mock.ec2.Vpc {} } else { mock.ec2.Subnet {} } }",
        "if c { if d { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} } } else { mock.ec2.Vpc {} }",
        "if c { if d { read mock.ec2.Vpc {} } else { read mock.ec2.Vpc {} } } else { read mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
    let invalid = "let x = if c { mock.ec2.Vpc {} } else if d { mock.ec2.Vpc {} }";
    assert!(
        carina_core::parser::parse_syntax_inventory(invalid, std::path::Path::new("main.crn"))
            .is_err(),
        "else if requires a braced else body"
    );
    assert_sibling_selector_source(invalid, None);
}

#[test]
fn parenthesized_resource_binding_is_untyped() {
    for rhs in [
        "(mock.ec2.Vpc {})",
        "((mock.ec2.Vpc {}))",
        "(\n  mock.ec2.Vpc {}\n)",
        "(if c { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} })",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn parenthesized_read_binding_is_untyped() {
    for rhs in ["(read mock.ec2.Vpc {})", "(read\n  mock.ec2.Vpc {})"] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn coalesced_resource_binding_is_untyped() {
    for rhs in [
        "mock.ec2.Vpc {} ?? mock.ec2.Vpc {}",
        "(mock.ec2.Vpc {}) ?? (mock.ec2.Vpc {}) ?? mock.ec2.Vpc {}",
        "(\n  mock.ec2.Vpc {}\n) ?? (\n  mock.ec2.Vpc {}\n)",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn coalesced_read_binding_is_untyped() {
    assert_sibling_selector("read mock.ec2.Vpc {} ?? read mock.ec2.Vpc {}", None);
}

#[test]
fn coalesced_non_resource_operand_does_not_type_resource_binding() {
    for rhs in [
        "y ?? mock.ec2.Vpc {}",
        "mock.ec2.Vpc {} ?? y",
        "y ?? mock.ec2.Vpc {} ?? z",
        "'default' ?? (mock.ec2.Vpc {})",
        "'mock.ec2.Subnet' ?? mock.ec2.Vpc {}",
        "\"${f('mock.ec2.Subnet')}\" ?? mock.ec2.Vpc {}",
        "<<EOT\nfallback\nEOT\n?? mock.ec2.Vpc {}",
        "(if c { y } else { z }) ?? mock.ec2.Vpc {}",
        "provider mock {} ?? mock.ec2.Vpc {}",
        "wait y { until = y.a == 'done' } ?? mock.ec2.Vpc {}",
        "(y ?? mock.ec2.Vpc {})",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn coalesced_different_resource_operands_are_untyped() {
    for rhs in [
        "mock.ec2.Vpc {} ?? mock.ec2.Subnet {}",
        "y ?? mock.ec2.Vpc {} ?? mock.ec2.Subnet {}",
        "mock.ec2.Vpc {} ?? read mock.ec2.Vpc {}",
        "read mock.ec2.Vpc {} ?? mock.ec2.Vpc {}",
        "(mock.ec2.Vpc {} ?? mock.ec2.Subnet {}) ?? mock.ec2.Vpc {}",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn resource_nested_in_non_resource_value_does_not_type_binding() {
    for rhs in [
        "[mock.ec2.Vpc {}]",
        "{ vpc = mock.ec2.Vpc {} }",
        "f(mock.ec2.Vpc {})",
        "mock.ec2.Vpc {}.shared",
        "for v in ['x'] { mock.ec2.Vpc {} }",
    ] {
        assert_sibling_selector(rhs, None);
    }
}

#[test]
fn unsaved_scalar_binding_shadows_sibling_resource() {
    let fixture = Fixture::new("let x = read mock.ec2.Vpc {}\n", "exports {}\n");
    let items = fixture.complete(
        "exports.crn",
        "let x = 'unsaved'\nattributes {\n  value = x.|\n}\n",
    );
    assert!(!labels(&items).contains(&"x.lookup_only"), "{items:?}");
}

#[test]
fn data_source_type_name_completion_inserts_read() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete("main.crn", "let img = |");
    let item = items
        .iter()
        .find(|item| item.label == "read mock.compute.Image")
        .expect("data source has a read label");
    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected snippet edit: {item:?}")
    };
    assert_eq!(
        edit.new_text,
        "read mock.compute.Image {\n    filter = ${1}\n}"
    );
    let expanded = format!("let img = {}", edit.new_text.replace("${1}", "'linux'"));
    assert!(
        carina_core::parser::parse_file(
            &expanded,
            std::path::Path::new("main.crn"),
            &ProviderContext::new(carina_core::EnvironmentSnapshot::empty())
        )
        .is_ok()
    );
}

#[test]
fn annotation_type_completion_uses_kindless_schema() {
    let fixture = Fixture::new("", "arguments {\n  x: mock.ec2.Vpc\n}\n");
    let items = fixture.complete("exports.crn", "arguments {\n  x: mock.|\n}\n");
    for (label, description) in [
        ("mock.ec2.Vpc", "Managed VPC reference"),
        ("mock.compute.Image", "Existing image reference"),
    ] {
        let matches = items
            .iter()
            .filter(|item| item.label == label)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "{label}: {matches:?}");
        assert_eq!(matches[0].detail.as_deref(), Some(description));
    }
}

#[test]
fn data_source_type_completion_preserves_existing_read() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete("main.crn", "let img = read mock.|");
    let item = items
        .iter()
        .find(|item| item.label == "read mock.compute.Image")
        .unwrap();
    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected snippet edit")
    };
    let expanded = format!(
        "let img = read {}",
        edit.new_text.replace("${1}", "'linux'")
    );
    let parsed = carina_core::parser::parse_file(
        &expanded,
        std::path::Path::new("main.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    );
    assert!(parsed.is_ok(), "{expanded}: {parsed:?}");
}

#[test]
fn shared_type_name_completions_have_distinct_kinds() {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete("main.crn", "let x = |");
    for (label, snippet) in [
        ("mock.ec2.Vpc", "mock.ec2.Vpc {\n}"),
        ("read mock.ec2.Vpc", "read mock.ec2.Vpc {\n}"),
    ] {
        let matches = items
            .iter()
            .filter(|item| item.label == label)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "{label}: {matches:?}");
        let Some(CompletionTextEdit::Edit(edit)) = &matches[0].text_edit else {
            panic!("expected snippet edit")
        };
        assert_eq!(edit.new_text, snippet);
    }
}

fn assert_anonymous_vpc_after(preceding: &str) {
    let source = format!("{preceding}\nmock.ec2.Vpc {{\n  shared = 'x'\n}}\n");
    let fixture = Fixture::new(&source, "exports {}\n");
    let items = fixture.complete(
        "main.crn",
        &format!("{preceding}\nmock.ec2.Vpc {{\n  |\n}}\n"),
    );
    assert!(labels(&items).contains(&"managed_only"), "{items:?}");
    let hover = fixture
        .hover("main.crn", &source.replace("shared", "sha|red"), true)
        .expect("anonymous resource attribute hover");
    assert!(hover.contains("Managed VPC"), "{hover}");

    let items = fixture.complete("exports.crn", "attributes {\n  value = s.|\n}\n");
    assert!(!labels(&items).contains(&"s.managed_only"), "{items:?}");
}

#[test]
fn walk_back_stops_after_multiline_list() {
    assert_anonymous_vpc_after("let cidrs = [\n  '10.0.0.0/16',\n]");
}

#[test]
fn walk_back_stops_after_multiline_call() {
    assert_anonymous_vpc_after("let x = join(\n  ['a', 'b'],\n  ','\n)");
}

#[test]
fn walk_back_stops_after_multiline_arithmetic() {
    assert_anonymous_vpc_after("let n =\n  1 + 2");
}

#[test]
fn walk_back_attribute_hover_after_completed_multiline_rhs() {
    let fixture = Fixture::new("", "exports {}\n");
    for preceding in [
        "let cidrs = [\n  '10.0.0.0/16',\n]",
        "let x = join(\n  ['a', 'b'],\n  ','\n)",
        "let n =\n  1 + 2",
    ] {
        let hover = fixture
            .hover(
                "main.crn",
                &format!("{preceding}\nmock.ec2.Vpc {{\n  sha|red = 'x'\n}}\n"),
                true,
            )
            .expect("anonymous resource attribute hover");
        assert!(hover.contains("Managed VPC"), "{hover}");
    }
}

#[test]
fn walk_back_stops_after_string_rhs_line() {
    assert_anonymous_vpc_after("let s =\n  'abc'");
}

#[test]
fn walk_back_stops_after_heredoc_rhs() {
    assert_anonymous_vpc_after("let s =\n  <<EOT\nx\nEOT");
}

#[test]
fn walk_back_stops_after_same_line_string_rhs() {
    assert_anonymous_vpc_after("let s = 'abc'");
}

#[test]
fn walk_back_read_is_word_bounded() {
    for preceding in ["thread", "foo.read", "unread", "_read"] {
        assert_anonymous_vpc_after(preceding);
    }
}

fn assert_sibling_binding_survives(buffer: &str) {
    let fixture = Fixture::new("", "exports {}\n");
    fixture.write("vpc.crn", "let v = mock.ec2.Vpc {}\n");
    let items = fixture.complete(
        "main.crn",
        &format!("{buffer}\nattributes {{\n  value = v.|\n}}\n"),
    );
    let labels = labels(&items);
    assert!(labels.contains(&"v.managed_only"), "{labels:?}");
    assert!(!labels.contains(&"v.lookup_only"), "{labels:?}");
}

#[test]
fn binding_scan_lexes_unterminated_string_per_file() {
    assert_sibling_binding_survives("let broken = '");
}

#[test]
fn binding_scan_lexes_unterminated_comment_per_file() {
    assert_sibling_binding_survives("/*");
}

#[test]
fn binding_scan_lexes_unterminated_heredoc_per_file() {
    assert_sibling_binding_survives("let broken = <<EOT");
}

#[test]
fn binding_scan_lexes_each_sibling_separately() {
    let fixture = Fixture::new("", "exports {}\n");
    fixture.write("broken.crn", "/*");
    fixture.write("vpc.crn", "let v = mock.ec2.Vpc {}\n");
    let items = fixture.complete("main.crn", "attributes {\n  value = v.|\n}\n");
    assert!(
        labels(&items).contains(&"v.managed_only"),
        "{:?}",
        labels(&items)
    );
}

#[test]
fn binding_scan_walk_back_stops_at_file_boundary() {
    let fixture = Fixture::new("", "exports {}\n");
    fixture.write("a.crn", "let s =\n");
    fixture.write("b.crn", "mock.ec2.Vpc {}\n");
    fixture.write("c.crn", "let v = mock.ec2.Vpc {}\n");
    let items = fixture.complete("main.crn", "attributes {\n  value = s.|\n}\n");
    assert!(
        !labels(&items).contains(&"s.managed_only"),
        "{:?}",
        labels(&items)
    );
    let items = fixture.complete("main.crn", "attributes {\n  value = v.|\n}\n");
    assert!(labels(&items).contains(&"v.managed_only"), "{items:?}");
}

fn completed_image_source(marked: &str, wrapped: bool, after_read: bool) -> String {
    let fixture = Fixture::new("", "exports {}\n");
    let items = fixture.complete("main.crn", marked);
    let item = items
        .iter()
        .find(|item| item.label == "read mock.compute.Image")
        .expect("data-source type completion");
    if after_read {
        assert_eq!(item.filter_text.as_deref(), Some("mock.compute.Image"));
    }
    let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
        panic!("expected snippet edit: {item:?}")
    };
    let binding_in_additional_edit = item
        .additional_text_edits
        .iter()
        .flatten()
        .any(|edit| edit.new_text == "let ${1:name} = ");
    let expected_start = if wrapped && !binding_in_additional_edit {
        "let ${1:name} = read mock.compute.Image {"
    } else if after_read {
        "mock.compute.Image {"
    } else {
        "read mock.compute.Image {"
    };
    assert!(edit.new_text.starts_with(expected_start), "{edit:?}");
    let source = marked.replacen('|', "", 1);
    let mut edits = vec![edit.clone()];
    edits.extend(item.additional_text_edits.iter().flatten().cloned());
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.range.start));
    let mut expanded = source.clone();
    for edit in edits {
        let start = carina_lsp::position::position_to_byte_offset(&source, edit.range.start);
        let end = carina_lsp::position::position_to_byte_offset(&source, edit.range.end);
        let replacement = edit
            .new_text
            .replace("${1:name}", "img")
            .replace("${1}", "'linux'")
            .replace("${2}", "'linux'");
        expanded.replace_range(start..end, &replacement);
    }
    let parsed = carina_core::parser::parse_file(
        &expanded,
        std::path::Path::new("main.crn"),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    );
    assert!(parsed.is_ok(), "{expanded}: {parsed:?}");
    expanded
}

#[test]
fn data_source_type_at_file_top_level_wraps_binding() {
    completed_image_source("mock.|", true, false);
}

#[test]
fn data_source_type_after_let_is_bare_expression() {
    completed_image_source("let img = mock.|", false, false);
}

#[test]
fn data_source_type_completion_applies_inside_existing_let_rhs() {
    for marked in ["let x = |", "let x = read |", "let x = mock.|"] {
        let expanded = completed_image_source(marked, false, marked.contains("read"));
        assert_eq!(
            expanded, "let x = read mock.compute.Image {\n    filter = 'linux'\n}",
            "{marked}"
        );
    }
}

fn assert_control_flow_body_completions(marked: &str) {
    let fixture = module_fixture();
    let items = fixture.complete("main.crn", marked);
    let mut actual = labels(&items);
    actual.sort_unstable();
    assert_eq!(
        actual,
        [
            "let",
            "local_module",
            "mock.ec2.Subnet",
            "mock.ec2.Vpc",
            "read",
            "read mock.compute.Image",
            "read mock.ec2.Vpc",
        ]
    );
    for (label, snippet) in [
        ("mock.ec2.Vpc", "mock.ec2.Vpc {\n}"),
        ("read mock.ec2.Vpc", "read mock.ec2.Vpc {\n}"),
        (
            "read mock.compute.Image",
            "read mock.compute.Image {\n    filter = ${1}\n}",
        ),
    ] {
        let item = items.iter().find(|item| item.label == label).unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("expected snippet edit: {item:?}")
        };
        assert_eq!(edit.new_text, snippet);
    }
    completed_image_source(marked, false, false);
}

#[test]
fn data_source_type_in_for_body_is_bare_expression() {
    assert_control_flow_body_completions("let imgs = for x in ['linux'] {\n  |\n}");
    completed_image_source("let imgs = for x in ['linux'] {\n  mock.|\n}", false, false);
}

#[test]
fn data_source_type_in_if_body_is_bare_expression() {
    assert_control_flow_body_completions("let img = if true {\n  |\n}");
    completed_image_source("let img = if true {\n  mock.|\n}", false, false);
}

#[test]
fn data_source_type_after_top_level_read_wraps_binding() {
    completed_image_source("read mock.|", true, true);
}

#[test]
fn typed_read_keeps_path_filter_range() {
    let fixture = Fixture::new("", "exports {}\n");
    for (marked, start, comment) in [
        ("read mock.|", Position::new(0, 5), None),
        ("read\n  mock.|", Position::new(1, 2), None),
        (
            "read // lookup\n  mock.|",
            Position::new(1, 2),
            Some("// lookup"),
        ),
        (
            "read /* lookup */ mock.|",
            Position::new(0, 18),
            Some("/* lookup */"),
        ),
    ] {
        let items = fixture.complete("main.crn", marked);
        let item = items
            .iter()
            .find(|item| item.label == "read mock.compute.Image")
            .unwrap();
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("expected snippet edit")
        };
        assert_eq!(edit.range.start, start);
        assert_eq!(item.filter_text.as_deref(), Some("mock.compute.Image"));
        let expanded = completed_image_source(marked, true, true);
        if let Some(comment) = comment {
            assert!(expanded.contains(comment), "{expanded}");
            assert!(expanded.starts_with("let img = read "), "{expanded}");
        }
    }
}

#[test]
fn data_source_type_after_bound_read_filters_by_path() {
    completed_image_source("let img = read mock.|", false, true);
}

#[test]
fn data_source_type_after_read_in_for_body_filters_by_path() {
    completed_image_source(
        "let imgs = for x in ['linux'] {\n  read mock.|\n}",
        false,
        true,
    );
}

#[test]
fn data_source_type_after_read_in_if_body_filters_by_path() {
    completed_image_source("let img = if true {\n  read mock.|\n}", false, true);
}

#[test]
fn typed_read_offers_only_data_source_types() {
    let fixture = Fixture::new("", "exports {}\n");
    for marked in [
        "read mock.|",
        "let img = read mock.|",
        "for x in ['linux'] {\n  read mock.|\n}",
        "if true {\n  read mock.|\n}",
    ] {
        let items = fixture.complete("main.crn", marked);
        assert_eq!(items.len(), 2, "{marked}: {:?}", labels(&items));
        assert!(
            items
                .iter()
                .all(|item| item.label.starts_with("read mock.")),
            "{:?}",
            labels(&items)
        );
    }
}
