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
