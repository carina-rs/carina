use super::*;
use crate::parser::retained::AuthoredCursor;
use crate::schema::AttributeType;

#[test]
fn ordinary_admission_findings_remain_terminal() {
    let error = constraints(vec![AdmissionFinding {
        path: LeafPath::from_segments([]),
        error: TypeError::MissingRequired {
            name: "ordinary-sentinel".into(),
        },
    }]);
    let rendered = crate::executor::render_error_chain(error.resolution_error());
    assert_eq!(
        rendered.matches("ordinary-sentinel").count(),
        1,
        "{rendered}"
    );
}

#[test]
fn conversion_admission_error_renders_its_typed_channel_once() {
    tests::fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, schemas, _, _| {
            let parsed = crate::parser::parse_file(
                "let sink = test.Sink { target = 'ok' as String }",
                std::path::Path::new("terminal.crn"),
                catalog,
            )
            .unwrap()
            .resolve_root_for_test();
            let configuration = crate::config_loader::load_buffer_configuration(
                crate::config_loader::issue_buffer_configuration(&parsed),
                schemas,
                catalog,
            );
            let bindings =
                BindingIndex::from_parsed(configuration.judged().file(), schemas, catalog);
            let presented = crate::validation::resource_admission_findings(
                configuration.judged(),
                &bindings,
                &std::collections::HashSet::from(["test".into()]),
                &mut Vec::new(),
            )
            .unwrap();
            assert_eq!(presented.len(), 1);
            let crate::validation::PresentedFinding::Conversion(conversion) =
                presented.iter().next().unwrap()
            else {
                panic!("typed presentation required");
            };
            let rendered = crate::test_utils::render_presented_for_test(&presented);
            assert_eq!(
                rendered.matches(&conversion.error().to_string()).count(),
                1,
                "{rendered}"
            );
            assert_eq!(
                conversion.location().file().path(),
                std::path::Path::new("terminal.crn")
            );
        },
    );
}

fn mismatched_cursor_is_rejected(apply: bool) {
    tests::fixture(
        AttributeType::string(),
        AttributeType::string(),
        |catalog, schemas, _, destination| {
            let parsed = crate::parser::parse_file(
                "test.Sink { target = 'ok' }",
                std::path::Path::new("phase.crn"),
                catalog,
            )
            .unwrap()
            .resolve_root_for_test();
            let configuration = crate::config_loader::load_buffer_configuration(
                crate::config_loader::issue_buffer_configuration(&parsed),
                schemas,
                catalog,
            );
            let tree =
                crate::parser::AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
            let resource = tree.resources().next().unwrap();
            let slot = resource.authored_slot("target").unwrap();
            let value: Value = Value::Concrete(crate::resource::ConcreteValue::String("ok".into()));
            let resolved = ResolvedBindings::default();
            crate::validation::deferred_types::with_session_for_test(catalog, |session| {
                for authored in [false, true] {
                    let (candidate, mut context) = if apply {
                        Candidate::apply(
                            value
                                .knowledge(&KnowledgeContext::apply(&resolved))
                                .unwrap(),
                            session,
                        )
                        .unwrap()
                    } else {
                        Candidate::plan(
                            PlanResolved::try_from(
                                value.knowledge(&KnowledgeContext::plan(&resolved)).unwrap(),
                            )
                            .unwrap(),
                            session,
                        )
                    };
                    context.cursor = if authored {
                        AuthoredCursor::authored(slot.cursor().unwrap())
                    } else {
                        AuthoredCursor::validation_projected(
                            crate::parser::retained::ValidationProjection::checked(
                                crate::test_utils::no_conversion_proof(),
                            ),
                        )
                    };
                    let error = admit(destination, candidate, &context)
                        .expect_err("proof cannot borrow its cursor's unrelated phase");
                    assert!(
                        error
                            .resolution_error()
                            .to_string()
                            .contains("projected callback/cursor pairing"),
                        "{error:?}"
                    );
                }
            });
        },
    );
}

#[test]
fn conversion_plan_proof_rejects_mismatched_cursor_without_panic() {
    mismatched_cursor_is_rejected(false);
}

#[test]
fn conversion_apply_proof_rejects_mismatched_cursor_without_panic() {
    mismatched_cursor_is_rejected(true);
}
