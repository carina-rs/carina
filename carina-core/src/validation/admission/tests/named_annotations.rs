use crate::binding_index::BindingIndex;
use crate::schema::AttributeType as DraftType;
use crate::validation::admission::{kind, tests::selected_destination};
use crate::validation::named_annotation_tests::{identity, member, snapshot};
use crate::validation::{deferred_types, resolve_file_type_exprs_report};
use std::{collections::HashMap, sync::Arc};

#[test]
fn wider_name_resource_reference_discharge_matches_the_destination_literal_error() {
    use crate::binding_index::{BindingValueSource, ResolvedBindings};
    use crate::resource::knowledge::KnowledgeContext;
    use crate::validation::admission::{Candidate, admit};
    let snapshot = snapshot(vec![(
        "p",
        vec![member(100, 128, None), member(1, 64, None)],
    )]);
    let context = snapshot.provider_context().as_provider_context();
    let parsed = crate::parser::parse_file(
        "let source = p.Values {}\nlet target = p.Values { value1 = source.value0 }\n",
        std::path::Path::new("main.crn"),
        context,
    )
    .unwrap()
    .resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, snapshot.schema_registry(), context);
    let destination = selected_destination::<kind::ResourceAttribute>(
        snapshot
            .schema_registry()
            .get(
                &parsed.resources[1].id.provider,
                &parsed.resources[1].id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .unwrap(),
        &parsed.resources[1].id,
        "value1",
        context,
    );
    deferred_types::with_session_for_test(snapshot.provider_context(), |session| {
        let reference = crate::resource::Value::root_resource_ref("source", "value0", vec![]);
        let (candidate, boundary) = Candidate::validation_classified(
            reference
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
            crate::test_utils::no_conversion_proof(),
        );
        let pending = admit(&destination, candidate, &boundary)
            .unwrap()
            .ready()
            .unwrap_err();
        let literal = crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
            "A".repeat(100).into(),
        ));
        let (candidate, boundary) = Candidate::validation_classified(
            literal
                .knowledge(&KnowledgeContext::validation())
                .unwrap()
                .into_classification(),
            Some(&bindings),
            session,
            crate::test_utils::no_conversion_proof(),
        );
        let literal_error = admit(&destination, candidate, &boundary).unwrap_err();
        let mut resolved = ResolvedBindings::default();
        resolved.set(
            "source",
            HashMap::from([("value0".into(), literal)]),
            BindingValueSource::Local,
        );
        let error = pending.discharge(&resolved, session).unwrap_err();
        assert_eq!(error.resolution_error(), literal_error.resolution_error());
        assert_eq!(
            error
                .resolution_error()
                .to_string()
                .matches("value1:")
                .count(),
            1
        );
    });
}

#[test]
fn round12_unknown_function_return_rechecks_destination_member_at_resolution() {
    use crate::binding_index::{BindingValueSource, ResolvedBindings};
    use crate::resource::knowledge::KnowledgeContext;
    use crate::validation::admission::{Candidate, admit};
    let slot = |member_length| {
        DraftType::string_member(
            crate::schema::StringIdentityLayer::new(
                Some(identity()),
                None,
                Some((Some(1), Some(64))),
                None,
            ),
            crate::schema::StringMemberConstraints::new(None, member_length),
        )
    };
    let loaded = snapshot(vec![(
        "p",
        vec![slot(None), slot(Some((Some(1), Some(8))))],
    )]);
    let context = loaded.provider_context().as_provider_context();
    let mut authored = crate::parser::parse("let source = p.Values {}\nlet target = p.Values {}\nfn forward(value: aws.iam.Role.Name): aws.iam.Role.Name { value }\n", context).unwrap();
    deferred_types::with_session_for_test(loaded.provider_context(), |session| {
        resolve_file_type_exprs_report(&mut authored, session, None)
    })
    .unwrap();
    let parsed = authored.resolve_root_for_test();
    let bindings = BindingIndex::from_parsed(&parsed, loaded.schema_registry(), context);
    let destination = selected_destination::<kind::ResourceAttribute>(
        loaded
            .schema_registry()
            .get(
                &parsed.resources[1].id.provider,
                &parsed.resources[1].id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
            .unwrap(),
        &parsed.resources[1].id,
        "value1",
        context,
    );
    for (length, accepted) in [(7, true), (20, false)] {
        let value = crate::resource::Value::Deferred(
            crate::resource::DeferredValue::ResolvedFunctionCall {
                function: Arc::new(parsed.user_functions["forward"].clone()),
                functions: Arc::new(parsed.user_functions.clone()),
                args: vec![crate::resource::FunctionArgument::from_value(
                    crate::resource::Value::root_resource_ref("source", "value0", vec![]),
                )],
                source_line: 1.into(),
            },
        );
        deferred_types::with_session_for_test(loaded.provider_context(), |session| {
            let (candidate, boundary) = Candidate::validation_classified(
                value
                    .knowledge(&KnowledgeContext::validation())
                    .unwrap()
                    .into_classification(),
                Some(&bindings),
                session,
                crate::test_utils::no_conversion_proof(),
            );
            let pending = admit(&destination, candidate, &boundary)
                .unwrap()
                .ready()
                .unwrap_err();
            let mut resolved = ResolvedBindings::default();
            resolved.set(
                "source",
                HashMap::from([(
                    "value0".into(),
                    crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                        "A".repeat(length).into(),
                    )),
                )]),
                BindingValueSource::Local,
            );
            let result = pending.discharge(&resolved, session);
            assert_eq!(result.is_ok(), accepted, "{result:?}");
            if let Err(error) = result {
                let message = error.resolution_error().to_string();
                assert!(
                    message.contains("outside allowed range") || message.contains("length"),
                    "{message}"
                );
                assert_eq!(message.matches("value1:").count(), 1, "{message}");
            }
        });
    }
}
