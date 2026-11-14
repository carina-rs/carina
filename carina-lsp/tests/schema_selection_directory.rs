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
    let source = "arguments {\n  sg:\n    mock.ec2.Vpc {\n      sha|red\n    }\n}\n";
    assert!(fixture.hover("main.crn", source, true).is_none());
    let items = fixture.complete("main.crn", source);
    assert!(!labels(&items).contains(&"managed_only"), "{items:?}");
}

#[test]
fn walk_back_assignment_is_not_resource_header() {
    let fixture = Fixture::new("", "exports {}\n");
    let source = "attr =\n  mock.ec2.Vpc {\n    sha|red\n  }\n";
    assert!(fixture.hover("main.crn", source, true).is_none());
    let items = fixture.complete("main.crn", source);
    assert!(!labels(&items).contains(&"managed_only"), "{items:?}");
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

#[test]
fn conditional_binding_is_not_a_resource_binding() {
    let fixture = Fixture::new(
        "let x = if true { mock.ec2.Vpc {} } else { mock.ec2.Vpc {} }\n",
        "exports {}\n",
    );
    let items = fixture.complete("exports.crn", "attributes {\n  value = x.|\n}\n");
    assert!(!labels(&items).contains(&"x.managed_only"), "{items:?}");
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
