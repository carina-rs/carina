//! End-to-end validation regressions for carina#3714.
//!
//! Every test drives the in-process `carina validate` pipeline against a
//! directory. Multi-file cases deliberately split declarations across sibling
//! `.crn` files so the loader merge seam is exercised directly.

use std::collections::HashMap;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use tempfile::TempDir;

struct DuplicateTestFactory;

impl ProviderFactory for DuplicateTestFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Duplicate declaration test provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { panic!("validation tests never instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("test.Thing")
                .attribute(AttributeSchema::new("name", AttributeType::string()).required())
                .with_unique_name_attribute("name"),
        ]
    }
}

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(DuplicateTestFactory)]
}

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, source) in files {
        std::fs::write(dir.path().join(name), source).expect("write fixture");
    }
    dir
}

fn validate(fixture: &TempDir) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(fixture.path(), factories())
}

fn duplicate_errors(diagnostics: &[String]) -> Vec<&str> {
    diagnostics
        .iter()
        .map(String::as_str)
        .filter(|message| message.contains("duplicate "))
        .collect()
}

#[test]
fn cross_file_duplicate_export_is_one_error_with_both_files() {
    let fixture = write_fixture(&[
        ("a.crn", "exports {\n  x = \"from-a\"\n}\n"),
        ("b.crn", "exports {\n  x = \"from-b\"\n}\n"),
    ]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate export 'x': declared in a.crn and b.crn"]
    );
}

#[test]
fn cross_file_duplicate_value_let_is_one_error_with_both_files() {
    let fixture = write_fixture(&[("a.crn", "let v = \"a\"\n"), ("b.crn", "let v = \"b\"\n")]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate binding 'v': declared in a.crn and b.crn"]
    );
}

#[test]
fn cross_file_duplicate_use_alias_is_one_binding_error() {
    let fixture = write_fixture(&[
        ("a.crn", "let shared = use { source = \"./module\" }\n"),
        ("b.crn", "let shared = use { source = \"./module\" }\n"),
    ]);
    let module = fixture.path().join("module");
    std::fs::create_dir(&module).expect("module dir");
    std::fs::write(module.join("main.crn"), "# empty module\n").expect("module fixture");

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
    );
}

#[test]
fn cross_file_duplicate_upstream_state_binding_is_one_error() {
    let fixture = write_fixture(&[
        (
            "a.crn",
            "let shared = upstream_state { source = \"./upstream\" }\n",
        ),
        (
            "b.crn",
            "let shared = upstream_state { source = \"./upstream\" }\n",
        ),
    ]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate binding 'shared': declared in a.crn and b.crn"]
    );
}

#[test]
fn duplicate_export_across_two_blocks_in_one_file_is_an_error() {
    let fixture = write_fixture(&[(
        "main.crn",
        "exports {\n  x = \"one\"\n}\n\nexports {\n  x = \"two\"\n}\n",
    )]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate export 'x': declared 2 times in main.crn"]
    );
}

#[test]
fn duplicate_export_inside_one_block_is_an_error() {
    let fixture = write_fixture(&[("main.crn", "exports {\n  x = \"one\"\n  x = \"two\"\n}\n")]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        diagnostics,
        vec!["duplicate export 'x': declared 2 times in main.crn"]
    );
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("last value")),
        "an export collision is an error, not a last-write-wins warning: {diagnostics:#?}",
    );
}

#[test]
fn duplicate_arguments_inside_one_block_are_an_error() {
    let fixture = write_fixture(&[(
        "main.crn",
        "arguments {\n  a: String = \"one\"\n  a: String = \"two\"\n}\n",
    )]);

    let diagnostics = validate(&fixture);

    assert_eq!(
        duplicate_errors(&diagnostics),
        vec!["duplicate argument 'a': declared 2 times in main.crn"]
    );
}

#[test]
fn module_sibling_files_cannot_redeclare_an_attribute() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module = temp.path().join("module");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&module).expect("module dir");
    std::fs::create_dir(&caller).expect("caller dir");
    std::fs::write(
        module.join("a.crn"),
        "attributes {\n  shared = \"from-a\"\n}\n",
    )
    .expect("module a");
    std::fs::write(
        module.join("b.crn"),
        "attributes {\n  shared = \"from-b\"\n}\n",
    )
    .expect("module b");
    std::fs::write(
        caller.join("main.crn"),
        "let component = use { source = '../module' }\n\nlet instance = component { }\n",
    )
    .expect("caller");

    let diagnostics = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert_eq!(
        duplicate_errors(&diagnostics),
        vec!["../module: duplicate module attribute 'shared': declared in a.crn and b.crn"]
    );
}

#[test]
fn module_argument_and_internal_binding_share_one_namespace_across_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module = temp.path().join("module");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&module).expect("module dir");
    std::fs::create_dir(&caller).expect("caller dir");
    std::fs::write(
        module.join("a.crn"),
        "arguments {\n  shared: String = \"argument\"\n}\n",
    )
    .expect("module argument");
    std::fs::write(module.join("b.crn"), "let shared = \"binding\"\n").expect("module binding");
    std::fs::write(
        caller.join("main.crn"),
        "let component = use { source = '../module' }\n\nlet instance = component { }\n",
    )
    .expect("caller");

    let diagnostics = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert_eq!(
        diagnostics,
        vec![
            "../module: duplicate name 'shared': declared as argument in a.crn and as binding in b.crn"
        ]
    );
}

#[test]
fn same_name_in_export_and_binding_namespaces_is_clean() {
    let fixture = write_fixture(&[(
        "main.crn",
        "let x = \"binding\"\n\nexports {\n  x = \"export\"\n}\n",
    )]);

    let diagnostics = validate(&fixture);

    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn resource_binding_variable_echo_is_not_a_duplicate() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"provider mock { }

let echoed = mock.test.Thing {
  name = "echoed"
}
"#,
    )]);

    let diagnostics = validate(&fixture);

    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}

#[test]
fn distinct_declaration_names_are_clean() {
    let fixture = write_fixture(&[
        ("a.crn", "let first = \"a\"\nexports {\n  left = \"a\"\n}\n"),
        (
            "b.crn",
            "let second = \"b\"\nexports {\n  right = \"b\"\n}\n",
        ),
    ]);

    let diagnostics = validate(&fixture);

    assert!(
        diagnostics.is_empty(),
        "unexpected diagnostics: {diagnostics:#?}"
    );
}
