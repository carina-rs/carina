use super::*;
use crate::parser::retained::{AuthoredCursor, ProjectedPhase};
use crate::schema::AttributeType;

#[test]
fn conversion_admission_error_renders_its_typed_channel_once() {
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_file(
        "let source = 'ok'",
        std::path::Path::new("terminal.crn"),
        &catalog,
    )
    .unwrap();
    let location = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&crate::resource::RelativeBindingName::new("source"))
        .unwrap()
        .location()
        .clone();
    let conversion = crate::ConversionError::new(
        crate::ConversionErrorKind::Source,
        &location,
        "admission-conversion-sentinel",
    );
    let error = constraints_with_conversions(Vec::new(), std::iter::once(conversion).collect());
    let rendered = crate::executor::render_error_chain(&error);
    assert_eq!(
        rendered.matches("admission-conversion-sentinel").count(),
        1,
        "{rendered}"
    );
    assert_eq!(
        crate::ConversionError::from_error(&error)
            .unwrap()
            .location(),
        &location
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
            let configuration =
                crate::config_loader::load_buffer_configuration(&parsed, schemas, catalog);
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
                        let validation = value
                            .knowledge(&KnowledgeContext::validation())
                            .unwrap()
                            .into_classification();
                        AuthoredCursor::projected(ProjectedPhase::classified(&validation))
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
