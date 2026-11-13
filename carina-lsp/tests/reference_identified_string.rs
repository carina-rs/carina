#[path = "../../carina-core/tests/support/reference_identified_string.rs"]
mod fixture;
mod support;

use support::fixture::{analyze, engine_with_schemas};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

fn assert_shape(wanted: &str) {
    let (shape, ty, value) = fixture::shapes()
        .into_iter()
        .find(|(shape, _, _)| *shape == wanted)
        .expect("registered nesting position");
    let engine = engine_with_schemas(fixture::schemas(ty));
    let main = fixture::consumer(value);
    let directory = fixture::directory(&main, fixture::UNKNOWN_SOURCE);
    for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
        std::fs::write(directory.path().join("sources.crn"), source).unwrap();
        assert_eq!(
            analyze(&engine, &directory, "main.crn"),
            [Diagnostic {
                range: Range::new(Position::new(1, 2), Position::new(1, 7)),
                severity: Some(DiagnosticSeverity::WARNING),
                source: Some("carina".into()),
                message: fixture::lsp_message(shape).into(),
                ..Default::default()
            }],
            "{shape}: {source}{main}"
        );
        for file in ["sources.crn", "providers.crn", "backend.crn"] {
            assert_eq!(analyze(&engine, &directory, file), [], "{shape}: {file}");
        }
    }
}

macro_rules! shape_test {
    ($name:ident, $shape:literal) => {
        #[test]
        fn $name() {
            assert_shape($shape);
        }
    };
}
shape_test!(issue_4384_scalar, "scalar");
shape_test!(issue_4384_list_element, "list");
shape_test!(issue_4384_map_value, "map");
shape_test!(issue_4384_struct_field, "struct");
shape_test!(issue_4384_inside_secret, "secret");
shape_test!(issue_4384_secret_container, "secret_list");
shape_test!(issue_4384_behind_ref, "ref");
shape_test!(issue_4384_recursive_definition, "recursive");
shape_test!(issue_4384_recursive_child, "recursive_union");
shape_test!(issue_4384_list_in_union, "union_list");
shape_test!(issue_4384_map_in_union, "union_map");
shape_test!(issue_4384_struct_in_union, "union_struct");

#[test]
fn issue_4384_differently_identified_string_and_reference_inputs_have_no_diagnostics() {
    for (kind, source_type) in fixture::differently_identified_reference_input_sources() {
        let engine = engine_with_schemas(fixture::differently_identified_reference_input_schemas(
            source_type,
        ));
        let main = fixture::consumer("source.value");
        let directory = fixture::directory(&main, "");
        for source in [
            "let source = mock.InputSource {}\n",
            "let source = mock.InputSource { value = 'k' }\n",
        ] {
            std::fs::write(directory.path().join("sources.crn"), source).unwrap();
            for file in ["main.crn", "sources.crn", "providers.crn", "backend.crn"] {
                assert_eq!(
                    analyze(&engine, &directory, file),
                    [],
                    "{kind}: {file}: {source}{main}"
                );
            }
        }
    }
}

#[test]
fn issue_4384_reference_identityless_same_identity_and_reference_sinks_have_no_diagnostics() {
    for (control, target, reference) in fixture::accepted_targets() {
        for (shape, ty, expression) in fixture::shapes_for(target.clone()) {
            let engine = engine_with_schemas(fixture::schemas_with_target(ty, target.clone()));
            let main = fixture::consumer(&expression.replace("r.key", reference));
            let directory = fixture::directory(&main, fixture::UNKNOWN_SOURCE);
            for source in [fixture::UNKNOWN_SOURCE, fixture::KNOWN_SOURCE] {
                std::fs::write(directory.path().join("sources.crn"), source).unwrap();
                for file in ["main.crn", "sources.crn", "providers.crn", "backend.crn"] {
                    assert_eq!(
                        analyze(&engine, &directory, file),
                        [],
                        "{control}: {shape}: {file}: {source}{main}"
                    );
                }
            }
        }
    }
}

async fn assert_reference_completions(
    target: carina_core::schema::AttributeType,
    module: Option<&str>,
    expected: bool,
) {
    let schemas = fixture::schemas(target);
    let directory = fixture::directory(
        "",
        &format!(
            "{}let up = upstream_state {{ source = './upstream' }}\n",
            fixture::UNKNOWN_SOURCE
        ),
    );
    std::fs::create_dir(directory.path().join("upstream")).unwrap();
    std::fs::write(
        directory.path().join("upstream/providers.crn"),
        "provider mock {}\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("upstream/exports.crn"),
        "exports { key = r.key }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("upstream/sources.crn"),
        fixture::UNKNOWN_SOURCE,
    )
    .unwrap();
    let (main, line) = if let Some(ty) = module {
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            format!("arguments {{ value: {ty} }}\n"),
        )
        .unwrap();
        (
            "let template = use { source = './child' }\nlet call = template {\n  value = \n}\n",
            2,
        )
    } else {
        ("mock.test.recovery_probe {\n  value = \n}\n", 1)
    };
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
    let mut client = support::TestClient::with_schemas(directory.path(), schemas).await;
    let uri = tower_lsp::lsp_types::Url::from_file_path(directory.path().join("main.crn")).unwrap();
    client.open_document(uri.as_str(), main).await;
    client.wait_for_diagnostics(uri.as_str()).await;
    let response = client._request_completion(uri.as_str(), line, 10).await;
    let items = response["result"].as_array().expect("completion result");
    let labels: Vec<_> = items
        .iter()
        .filter_map(|item| item["label"].as_str())
        .collect();
    // Unannotated upstream exports are not source-typed at validate yet: carina#4377.
    for label in ["r.key", "up.key"] {
        assert_eq!(
            labels.contains(&label),
            expected,
            "{module:?}: {label}: {labels:#?}"
        );
    }
    client.shutdown().await;
}

#[tokio::test]
async fn issue_4384_resource_completion_filters_reference_and_upstream_identity() {
    for (target, expected) in [
        (
            fixture::identified_string(carina_core::schema::TypeIdentity::from_dotted(
                "mock.RecoveryProbe",
            )),
            false,
        ),
        (fixture::identified_string(fixture::key_identity()), true),
    ] {
        assert_reference_completions(target, None, expected).await;
    }
}

#[tokio::test]
async fn issue_4384_typed_completion_keeps_same_identity_and_plain_string_controls() {
    for (ty, expected) in [
        ("mock.RecoveryProbe", false),
        ("mock.keys.Key.Reference", true),
        ("String", true),
    ] {
        assert_reference_completions(
            fixture::identified_string(carina_core::schema::TypeIdentity::from_dotted(
                "mock.RecoveryProbe",
            )),
            Some(ty),
            expected,
        )
        .await;
    }
}

#[tokio::test]
async fn issue_4384_plain_string_field_completion_keeps_reference_candidate() {
    use carina_core::schema::{AttributeType, StructField};
    let target = AttributeType::struct_(
        "Record",
        vec![StructField::required("field", AttributeType::string()).build()],
    );
    let main = "mock.test.recovery_probe {\n  value = {\n    field = \n  }\n}\n";
    let directory = fixture::directory(main, fixture::UNKNOWN_SOURCE);
    let mut client =
        support::TestClient::with_schemas(directory.path(), fixture::schemas(target)).await;
    let uri = tower_lsp::lsp_types::Url::from_file_path(directory.path().join("main.crn")).unwrap();
    client.open_document(uri.as_str(), main).await;
    client.wait_for_diagnostics(uri.as_str()).await;
    let response = client._request_completion(uri.as_str(), 2, 12).await;
    let items = response["result"].as_array().expect("completion result");
    assert!(
        items.iter().any(|item| item["label"] == "r.key"),
        "{response:#?}"
    );
    client.shutdown().await;
}
