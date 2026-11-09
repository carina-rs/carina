//! Acceptance controls shared with the CLI value-knowledge directory fixtures.

mod support;

use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use support::fixture::{analyze, engine_with_schemas};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

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
fn failed_secret_required_attribute_matches_unwrapped_diagnostic_order() {
    const MISSING: &str = "CARINA_4316_UNUSED_MISSING";
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../carina-cli/tests/fixtures/value_knowledge/required_failure");
    for protected in [false, true] {
        let fixture = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(&source).unwrap() {
            let entry = entry.unwrap();
            std::fs::copy(entry.path(), fixture.path().join(entry.file_name())).unwrap();
        }
        if !protected {
            let path = fixture.path().join("main.crn");
            let text = std::fs::read_to_string(&path).unwrap().replace(
                &format!("secret(env(\"{MISSING}\"))"),
                &format!("env(\"{MISSING}\")"),
            );
            std::fs::write(path, text).unwrap();
        }
        let engine = engine_with_schemas(schemas());
        let findings = analyze(&engine, &fixture, "main.crn");
        let cause = if protected {
            carina_core::resource::INSIDE_SECRET_MESSAGE.to_string()
        } else {
            format!("env(): environment variable '{MISSING}' is not set")
        };
        let strict_cause = format!("Invalid expression at line 1: {cause}");
        let actual: Vec<_> = findings
            .iter()
            .map(|finding| (finding.severity, finding.message.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                (Some(DiagnosticSeverity::ERROR), cause.as_str()),
                (Some(DiagnosticSeverity::ERROR), strict_cause.as_str()),
                (
                    Some(DiagnosticSeverity::WARNING),
                    "Type mismatch: expected Map<String>, got Int",
                ),
            ],
            "protected={protected}: {findings:#?}",
        );
        let root_range = Range::new(
            Position::new(0, 0),
            Position::new(0, if protected { 87 } else { 79 }),
        );
        let tag_start = if protected { 77 } else { 69 };
        assert_eq!(
            findings
                .iter()
                .map(|finding| (finding.range, finding.source.as_deref()))
                .collect::<Vec<_>>(),
            [
                (root_range, Some("carina")),
                (root_range, Some("carina")),
                (
                    Range::new(Position::new(0, tag_start), Position::new(0, tag_start + 4)),
                    Some("carina"),
                ),
            ],
        );
        // carina#4337: main also reports the failing site twice; preserve that count here.
        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
                .count(),
            2,
        );
        assert!(
            !findings
                .iter()
                .any(|finding| finding.message.contains("Missing required")),
            "{findings:#?}"
        );
        if protected {
            let rendered = format!("{findings:?}");
            assert!(rendered.contains(carina_core::resource::INSIDE_SECRET_MESSAGE));
            assert!(!rendered.contains(MISSING), "{rendered}");
            assert!(!rendered.contains("secret(env("), "{rendered}");
        }
    }
}
