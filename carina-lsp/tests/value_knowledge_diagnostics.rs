//! Acceptance controls shared with the CLI value-knowledge directory fixtures.

mod support;

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use support::fixture::{analyze, engine_with_schemas};
use tower_lsp::lsp_types::DiagnosticSeverity;

fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::required("name", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("parent_name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .with_unique_name_attribute("name")
            .validate()
            .unwrap(),
    );
    schemas
}

fn assert_shared_fixture_accepted(group: &str, name: &str) {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-cli/tests/fixtures")
        .join(group)
        .join(name);
    let fixture = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), fixture.path().join(entry.file_name())).unwrap();
    }
    if name == "partial_coalesced" {
        let up = fixture.path().join("up");
        std::fs::create_dir(&up).unwrap();
        for (name, text) in [
            (
                "main.crn",
                r#"let seed = mock.test.resource { name = "seed" }
"#,
            ),
            ("providers.crn", "provider mock {}\n"),
            ("exports.crn", "exports {}\n"),
        ] {
            std::fs::write(up.join(name), text).unwrap();
        }
    }
    let engine = engine_with_schemas(schemas());
    for file in ["main.crn", "providers.crn", "exports.crn"] {
        let errors = analyze(&engine, &fixture, file)
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
            .collect::<Vec<_>>();
        assert!(errors.is_empty(), "{name}/{file}: {errors:#?}");
    }
}

#[test]
fn lexical_resource_function_keeps_directory_visibility() {
    assert_shared_fixture_accepted("value_knowledge", "lexical_resource");
}

#[test]
fn sibling_functions_keep_directory_visibility() {
    assert_shared_fixture_accepted("value_knowledge", "lexical_sibling");
}

#[test]
fn apply_only_argument_remains_valid_before_publication() {
    assert_shared_fixture_accepted("value_knowledge", "lexical_apply");
}

#[test]
fn managed_loop_with_unknown_count_remains_valid() {
    assert_shared_fixture_accepted("value_knowledge", "unknown_count");
}

#[test]
fn unselected_failing_fallback_remains_lazy() {
    assert_shared_fixture_accepted("value_knowledge", "lazy_fallback");
}

#[test]
fn partial_collection_keeps_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "partial_direct");
}
#[test]
fn partial_fallback_keeps_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "partial_coalesced");
}
#[test]
fn flattened_partial_shapes_keep_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "shape_functions");
}

#[test]
fn protected_interpolation_keeps_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "protected_interpolation");
}

#[test]
fn protected_builtin_keeps_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "protected_builtin");
}

#[test]
fn protected_pending_iterable_keeps_directory_validation_parity() {
    assert_shared_fixture_accepted("value_knowledge", "protected_loop");
}

#[test]
fn pending_secrets_and_local_absence_keep_directory_parity() {
    for name in [
        "secret_parameter",
        "secret_return",
        "secret_coalesce",
        "complete_local_absence",
        "if_shape_length",
    ] {
        assert_shared_fixture_accepted("value_knowledge", name);
    }
}

#[test]
fn unavailable_required_attribute_keeps_main_sibling_finding() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-cli/tests/fixtures/value_knowledge/required_failure");
    let fixture = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), fixture.path().join(entry.file_name())).unwrap();
    }
    let engine = engine_with_schemas(schemas());
    let findings = analyze(&engine, &fixture, "main.crn");
    let relevant = findings
        .iter()
        .filter(|finding| {
            finding.message.contains("invalid value inside secret(...)")
                || finding.message.contains("expected Map")
        })
        .collect::<Vec<_>>();
    // Main retains the attempted failure until plan. The core Err collector
    // is separately pinned with Cycle; this directory pins Failed parity.
    assert_eq!(relevant.len(), 1, "{findings:#?}");
    assert!(
        !findings
            .iter()
            .any(|finding| finding.message.contains("Missing required")),
        "{findings:#?}"
    );
}
