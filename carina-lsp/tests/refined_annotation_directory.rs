#[path = "../../carina-core/tests/support/refined_annotations.rs"]
mod fixture;

use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
use std::sync::Arc;

#[test]
fn member_refined_directory_annotations_have_lsp_validate_parity() {
    for &(main, exports) in fixture::CASES.iter().chain(fixture::READ_CASES) {
        for accepted in [true, false] {
            let (main, exports) = if accepted {
                (main.into(), exports.into())
            } else {
                (
                    main.replace("registry_prod.account_id", "registry_prod.other_id"),
                    exports
                        .replace("registry_prod.account_id", "registry_prod.other_id")
                        .replace("b.account_id", "a.other_id"),
                )
            };
            let snapshot = Arc::new(fixture::snapshot());
            let dir = fixture::directory(&main, &exports);
            let checked = fixture::validate(dir.path(), &snapshot);
            assert_eq!(checked.is_ok(), accepted, "{main}\n{exports}\n{checked:?}");
            let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["awscc".into()]);
            let mut messages = Vec::new();
            for name in ["main.crn", "exports.crn"] {
                let path = dir.path().join(name);
                let document = Document::new_with_path(
                    std::fs::read_to_string(&path).unwrap(),
                    path.clone(),
                    Arc::new(snapshot.provider_context().as_provider_context().clone()),
                );
                messages.extend(
                    engine
                        .analyze(&document, Some(&path))
                        .into_iter()
                        .map(|d| d.message),
                );
            }
            let mismatches = messages
                .iter()
                .filter(|message| {
                    ((message.contains("expected")
                        || message.contains("collection element types disagree")
                        || message.contains("does not match left type"))
                        && message.contains("aws.AccountId"))
                        || message.contains("cannot assign aws.OtherId")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                mismatches.is_empty(),
                accepted,
                "{main}\n{exports}\n{messages:#?}"
            );
            assert!(
                !messages
                    .iter()
                    .any(|m| m.contains("expected aws.AccountId, got aws.AccountId")),
                "{messages:#?}"
            );
        }
    }
}

#[test]
fn round14_member_obligation_has_validate_and_lsp_parity() {
    let snapshot = Arc::new(fixture::snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["awscc".into()]);
    for length in [5, 50] {
        let main = format!(
            "fn make_name(): aws.Name {{ '{}' }}\nlet named = make_name()\nlet sink = awscc.MemberSink {{ name = named }}\n",
            "A".repeat(length)
        );
        let dir = fixture::directory(&main, "exports {}\n");
        let result = fixture::validate(dir.path(), &snapshot);
        assert_eq!(result.is_ok(), length == 5, "{result:?}");
        let path = dir.path().join("main.crn");
        let doc = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&doc, Some(&path));
        let constraints: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.message.contains("outside allowed range"))
            .collect();
        assert_eq!(
            constraints.len(),
            usize::from(length == 50),
            "{diagnostics:#?}"
        );
        if let Err(error) = result {
            assert_eq!(error.matches("outside allowed range").count(), 1, "{error}");
        }
    }
}

#[path = "../../carina-core/tests/support/refined_destinations.rs"]
mod destinations;

fn boundary_parity(boundary: destinations::Boundary) {
    let snapshot = Arc::new(fixture::snapshot());
    let engine = DiagnosticEngine::new(Arc::clone(&snapshot), vec!["awscc".into()]);
    for length in [None, Some(5), Some(50)] {
        let applied = destinations::check(boundary, &snapshot, length);
        assert_eq!(
            applied.is_ok(),
            length != Some(50),
            "{boundary:?}: {applied:?}"
        );
        // The editor has no apply phase: known literals are its resolved-value oracle.
        let dir = destinations::directory(boundary, length);
        let path = dir.path().join("main.crn");
        let doc = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(snapshot.provider_context().as_provider_context().clone()),
        );
        let diagnostics = engine.analyze(&doc, Some(&path));
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR))
            .collect();
        assert_eq!(
            errors.len(),
            usize::from(length == Some(50)),
            "{boundary:?}, {length:?}: {errors:#?}"
        );
        if let Some(error) = errors.first() {
            assert_eq!(
                error.message.matches("outside allowed range").count(),
                1,
                "{error:?}"
            );
        }
    }
}
macro_rules! boundary_test {
    ($name:ident, $boundary:ident) => {
        #[test]
        fn $name() {
            boundary_parity(destinations::Boundary::$boundary);
        }
    };
}
boundary_test!(round15_resource_attribute, Resource);
boundary_test!(round15_let_forwarding, Let);
boundary_test!(round15_export, Export);
boundary_test!(round15_module_argument, ModuleArgument);
boundary_test!(round15_function_parameter, FunctionParameter);
boundary_test!(round15_function_return, FunctionReturn);
boundary_test!(round15_attributes_block, Attributes);
