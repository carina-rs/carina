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
