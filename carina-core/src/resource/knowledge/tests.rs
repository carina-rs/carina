//! Internal visitor contracts; production visitors stay within Classification consumers.
use super::super::{ApplyTimeUnknown, PendingReRead, RelativeBindingName};
use super::*;
use std::convert::Infallible;
type V = Value<NoPlaceholder, RelativeBindingName>;
fn secret(value: V) -> V {
    Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
}

#[test]
fn partial_collection_keeps_membership_and_original_reference_selectors() {
    let reference = V::resource_ref("producer", "identifier", vec![]);
    let value = V::Concrete(ConcreteValue::Map(IndexMap::from([
        ("key.with.dots".into(), reference.clone()),
        ("known".into(), V::Concrete(ConcreteValue::Int(7))),
    ])));
    let Classification::ShapeKnown(shape) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("only the leaves need resolution");
    };
    assert_eq!(shape.len(), 2);
    assert!(
        shape
            .sites()
            .next()
            .unwrap()
            .inspect::<bool, Infallible>(|path, _, wait| Ok(path.segments().len() == 1
                && matches!(wait, WaitOn::Resource(path) if path.binding() == "producer")))
            .unwrap()
    );
    shape.visit(
        |view| match view {
            ShapeView::Map(entries) => {
                assert_eq!(
                    entries.keys().copied().collect::<Vec<_>>(),
                    ["key.with.dots", "known"]
                );
                assert_eq!(entries["key.with.dots"].to_value(), reference);
            }
            ShapeView::List(_) => panic!("map shape"),
        },
        |_| panic!("ordinary map"),
    );
    assert_eq!(PlanIterable::from_shape(shape).unwrap().len(), 2);
}

#[test]
fn known_children_are_proofs_and_known_scalars_cannot_be_iterables() {
    let list: V = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Int(7),
    )]));
    let Classification::Known(known) = list
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("complete list");
    };
    known.visit(
        |view| match view {
            KnownView::List(items) => {
                assert_eq!(items.len(), 1);
                items.iter().next().unwrap().visit(
                    |view| assert!(matches!(view, KnownView::Int(7))),
                    |_| panic!("ordinary int"),
                );
            }
            _ => panic!("known list"),
        },
        |_| panic!("ordinary list"),
    );
    let scalar: V = Value::Concrete(ConcreteValue::Bool(true));
    let Classification::Known(known) = scalar
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("known scalar");
    };
    assert!(PlanIterable::from_known(known).is_none());
}

#[test]
fn secret_never_dispatches_to_an_ordinary_callback_at_any_knowledge_level() {
    let protected = secret(V::Concrete(ConcreteValue::String(
        "classified-secret".into(),
    )));
    let Classification::Known(known) = protected
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("secret knowledge is transparent, data is protected");
    };
    known.visit(|_| panic!("plaintext view escaped"), |_| ());
    assert_eq!(known.to_value(), protected);

    let value: V = secret(V::Deferred(DeferredValue::Unknown(
        ApplyTimeUnknown::PendingReRead(PendingReRead::not_found()),
    )));
    let Classification::Pending(pending) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("protected pending");
    };
    pending.visit(
        |_, _, _| panic!("protected node in ordinary visitor"),
        |_| (),
    );
    assert!(pending.sites().next().is_some());
    assert!(
        pending
            .sites()
            .all(|site| site.scope == DiagnosticScope::Secret)
    );

    let value = secret(V::Concrete(ConcreteValue::List(vec![value])));
    let Classification::Pending(shape) = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap()
    else {
        panic!("protected shape")
    };
    shape.visit(
        |_, _, _| panic!("protected collection view escaped"),
        |_| (),
    );
    assert!(shape.sites().next().is_some());
    assert!(
        shape
            .sites()
            .all(|site| site.scope == DiagnosticScope::Secret)
    );
    assert!(!format!("{shape:?}").contains("classified-secret"));
}

#[test]
fn phase_paths_render_struct_fields_map_keys_and_indices_distinctly() {
    use crate::schema::{LeafPath, LeafPathSegment};
    let path = LeafPath::from_segments([
        LeafPathSegment::StructField("rules".into()),
        LeafPathSegment::MapKey("a.b".into()),
        LeafPathSegment::ListIndex(2),
    ]);
    assert_eq!(path.to_string(), "rules[\"a.b\"][2]");
}

#[test]
fn classified_path_selection_keeps_known_siblings_of_attempted_failures() {
    use crate::resource::{AccessMissPolicy, PathSegment, select_classified_segment};
    let raw: V = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        ("good".to_owned(), Value::Concrete(ConcreteValue::Int(7))),
        (
            "bad".to_owned(),
            Value::Deferred(DeferredValue::Unavailable {
                name: crate::builtins::BuiltinFunction::env,
                args: vec![],
                reason: crate::resource::UnavailableReason::from_builtin_error(
                    "attempted builtin failure".into(),
                ),
                source_line: 0.into(),
            }),
        ),
    ])));
    let proof = raw
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(matches!(
        proof.clone().into_known(),
        Err(KnowledgeError::Unavailable { .. })
    ));
    assert_eq!(
        select_classified_segment(
            (&proof).into(),
            PathSegment::Field {
                name: "good".into()
            },
            0,
            AccessMissPolicy::Error
        )
        .unwrap()
        .into_value()
        .unwrap(),
        Value::Concrete(ConcreteValue::Int(7))
    );
}

fn attempted_failure() -> V {
    V::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "attempted fallback failure".into(),
        ),
        source_line: 0.into(),
    })
}

fn apply_wait() -> V {
    V::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
        PendingReRead::not_found(),
    )))
}

fn coalesce(left: V, alternative: V) -> V {
    V::Deferred(DeferredValue::Coalesce {
        value: Box::new(left),
        default: Box::new(alternative),
    })
}

#[test]
fn pending_coalesce_keeps_failed_alternative_without_making_it_required() {
    let value = coalesce(apply_wait(), attempted_failure());
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let Classification::Pending(pending) = &proof else {
        panic!("coalesce waits for its discriminator")
    };
    let (_, alternative) = pending.coalesce().unwrap();
    assert!(matches!(alternative, Classification::Failed(_)));
    assert!(matches!(
        proof.clone().into_known(),
        Err(KnowledgeError::InvalidStage { .. })
    ));
    let resolved = PlanResolved::try_from(
        value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                &crate::binding_index::ResolvedBindings::default(),
            ))
            .unwrap(),
    )
    .unwrap();
    let Classification::Pending(pending) = resolved.knowledge() else {
        panic!("the alternative cannot become a value before selection")
    };
    let kernel::PendingNode::Coalesce(coalesce) = pending.node.as_ref() else {
        panic!("the failed alternative belongs to the pending coalesce")
    };
    assert!(matches!(
        coalesce.alternative.evidence.as_ref(),
        Classification::Failed(_)
    ));
    assert!(matches!(
        pending.to_value(),
        V::Deferred(DeferredValue::Coalesce { value, default })
            if matches!(*value, V::Deferred(DeferredValue::Unknown(_)))
                && matches!(*default, V::Deferred(DeferredValue::Unavailable { .. }))
    ));
    assert_eq!(pending.sites().count(), 2);
    assert!(pending.sites().all(|site| site.phase() == Phase::Apply));
}

#[test]
fn failed_coalesce_alternative_does_not_hide_a_plan_discriminator() {
    let value = coalesce(
        V::Deferred(DeferredValue::Interpolation(vec![])),
        attempted_failure(),
    );

    assert!(matches!(
        PlanResolved::try_from(value.knowledge(&crate::resource::knowledge::KnowledgeContext::plan(&crate::binding_index::ResolvedBindings::default())).unwrap()),
        Err(KnowledgeError::UnresolvedAtBoundary { sites })
            if sites.iter().all(|site| site.phase() == Phase::Plan)
    ));
}

#[test]
fn expansion_admits_pending_coalesces_with_failed_alternatives_as_members() {
    let member = coalesce(apply_wait(), attempted_failure());
    let value = V::Concrete(ConcreteValue::List(vec![member.clone()]));
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let Classification::ShapeKnown(shape) = proof.clone() else {
        panic!("the list membership is established")
    };
    let iterable = PlanIterable::from_shape(shape).unwrap();
    assert_eq!(iterable.len(), 1);
    let V::Concrete(ConcreteValue::List(members)) = iterable.for_expansion() else {
        panic!("expansion retains the list")
    };
    assert_eq!(members.len(), 1);
    assert!(matches!(
        &members[0],
        V::Deferred(DeferredValue::Coalesce { value, default })
            if matches!(value.as_ref(), V::Deferred(DeferredValue::Unknown(_)))
                && matches!(default.as_ref(), V::Deferred(DeferredValue::Unavailable { .. }))
    ));
    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );

    let value = coalesce(value, attempted_failure());
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let Classification::ShapeKnown(shape) = proof.clone() else {
        panic!("coalesce retains the established left shape")
    };
    assert_eq!(PlanIterable::from_shape(shape).unwrap().len(), 1);
    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );
}

#[test]
fn active_failed_members_and_required_expression_inputs_remain_fatal() {
    for value in [
        attempted_failure(),
        V::Concrete(ConcreteValue::List(vec![apply_wait(), attempted_failure()])),
        V::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![attempted_failure().into()],
            source_line: 0.into(),
        }),
        coalesce(attempted_failure(), apply_wait()),
        coalesce(
            V::Concrete(ConcreteValue::List(vec![attempted_failure()])),
            apply_wait(),
        ),
    ] {
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(matches!(
            proof.clone().into_known(),
            Err(KnowledgeError::Unavailable { .. })
        ));
        if let Classification::ShapeKnown(shape) = proof.clone() {
            assert!(matches!(
                PlanIterable::from_shape(shape),
                Err(KnowledgeError::Unavailable { .. })
            ));
        }
        assert!(matches!(
            PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            ),
            Err(KnowledgeError::Unavailable { .. })
        ));
    }
}

#[test]
fn nested_failed_alternative_keeps_its_failure_until_selected() {
    let alternative = V::Concrete(ConcreteValue::Map(IndexMap::from([(
        "failed.member".into(),
        attempted_failure(),
    )])));
    let value = coalesce(apply_wait(), alternative.clone());
    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );

    assert!(matches!(
        PlanResolved::try_from(
            alternative
                .knowledge(&KnowledgeContext::plan(&ResolvedBindings::default()))
                .unwrap()
        ),
        Err(KnowledgeError::Unavailable { .. })
    ));
}

#[test]
fn protected_coalesce_discriminators_keep_apply_timing_without_exposing_payloads() {
    for (left, collection) in [
        (secret(apply_wait()), false),
        (
            V::Concrete(ConcreteValue::List(vec![secret(apply_wait())])),
            true,
        ),
    ] {
        let opaque = left
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(match &opaque {
            Classification::Pending(_) => !collection,
            Classification::ShapeKnown(_) => collection,
            Classification::Known(_) | Classification::Failed(_) => false,
        });
        assert_eq!(opaque.sites().count(), 1);
        assert!(
            opaque
                .sites()
                .all(|site| site.scope == DiagnosticScope::Secret)
        );
        assert!(opaque.clone().into_known().is_err());
        let value = coalesce(left, attempted_failure());
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        assert!(proof.sites().all(|site| site.phase() == Phase::Apply));
        assert!(
            PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            )
            .is_ok()
        );
    }

    let value = coalesce(
        secret(V::Deferred(DeferredValue::Interpolation(vec![]))),
        attempted_failure(),
    );
    assert!(matches!(
        PlanResolved::try_from(
            value
                .knowledge(&KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        ),
        Err(KnowledgeError::UnresolvedAtBoundary { .. })
    ));
}

#[test]
fn protected_expression_operands_keep_apply_timing() {
    let hidden = secret(apply_wait());
    for value in [
        V::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("p-".into()),
            InterpolationPart::Expr(hidden.clone()),
        ])),
        V::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![hidden.clone().into()],
            source_line: 7.into(),
        }),
    ] {
        let proof = value
            .knowledge(&KnowledgeContext::validation())
            .map(|proof| proof.into_classification())
            .unwrap();
        let Classification::Pending(pending) = &proof else {
            panic!("pending expression")
        };
        assert_eq!(pending.phase(), Phase::Apply);
        assert!(
            PlanResolved::try_from(
                value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                        &crate::binding_index::ResolvedBindings::default()
                    ))
                    .unwrap()
            )
            .is_ok()
        );
    }
}

#[test]
fn protected_shape_operands_keep_apply_timing_when_the_shape_is_pending() {
    let value = V::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::length,
        args: vec![secret(apply_wait()).into()],
        source_line: 9.into(),
    });
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let Classification::Pending(pending) = &proof else {
        panic!("pending shape operation")
    };
    assert_eq!(pending.phase(), Phase::Apply);
    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );
}

#[test]
fn unresolved_plan_lookups_follow_main_retention_policy() {
    use crate::binding_index::{BindingCompleteness, BindingValueSource, ResolvedBindings};
    use std::collections::HashMap;
    let value = Value::root_resource_ref("missing", "output", vec![]);
    for upstream in [false, true] {
        let mut bindings = ResolvedBindings::default();
        if upstream {
            bindings.set_with_completeness(
                "missing",
                HashMap::new(),
                BindingValueSource::Upstream,
                BindingCompleteness::Complete,
            );
        }

        let proof = value.knowledge(&KnowledgeContext::plan(&bindings)).unwrap();
        assert!(PlanResolved::try_from(proof).is_ok());
    }
    let value: Value = Value::Deferred(DeferredValue::Interpolation(vec![]));
    assert!(matches!(
        PlanResolved::try_from(
            value
                .knowledge(&KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        ),
        Err(KnowledgeError::UnresolvedAtBoundary { .. })
    ));
}

#[test]
fn knowledge_diagnostics_keep_relative_boundary_sites_without_readdressing() {
    use crate::resolver::ResolutionError;
    let raw = V::resource_ref("producer", "identifier", vec![]);
    let cause = KnowledgeError::UnresolvedAtBoundary {
        sites: raw
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .sites()
            .collect(),
    };
    let error = ResolutionError::from(cause);
    assert_eq!(
        error.to_string(),
        "1 plan-time value operation(s) remain unresolved at the plan boundary"
    );
    let ResolutionError::Knowledge(error) = error else {
        panic!("typed knowledge cause")
    };
    let KnowledgeDiagnostic::Relative(KnowledgeError::UnresolvedAtBoundary { sites }) = *error
    else {
        panic!("retain the original binding domain")
    };
    assert_eq!(sites.len(), 1);
    assert!(
        sites[0]
            .inspect::<bool, Infallible>(|_, _, wait| {
                Ok(matches!(wait, WaitOn::Resource(path) if path.binding() == "producer"))
            })
            .unwrap()
    );
}

#[test]
fn contextual_knowledge_errors_retain_typed_terminal_causes_and_original_text() {
    use std::error::Error;
    let cause = KnowledgeError::<RelativeBindingName>::InvalidStage {
        operation: "known-value projection",
    };
    let schema = crate::schema::TypeError::from(cause.clone());
    assert_eq!(
        schema.to_string(),
        "Validation failed: known-value projection is not valid at this resolution boundary"
    );
    assert!(schema.source().is_none());
    assert!(
        matches!(schema, crate::schema::TypeError::Knowledge { error, .. }
        if matches!(*error, KnowledgeDiagnostic::Relative(KnowledgeError::InvalidStage { .. })))
    );

    let deferred =
        crate::parser::DeferredForExpansionError::resolution(7, "items".into(), cause.into());
    assert_eq!(
        deferred.to_string(),
        "deferred-for iterable `items` at line 7: known-value projection is not valid at this resolution boundary"
    );
    assert!(deferred.source().is_none());
    assert!(
        matches!(deferred, crate::parser::DeferredForExpansionError::Knowledge { error, .. }
        if matches!(*error, KnowledgeDiagnostic::Relative(KnowledgeError::InvalidStage { .. })))
    );
}

#[test]
fn shape_builtin_failure_keeps_its_typed_attempted_cause() {
    let raw: V = Value::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "attempted builtin failure".into(),
        ),
        source_line: 9.into(),
    });
    let classified = raw
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let error = crate::builtins::builtin_length(&classified).unwrap_err();
    assert_eq!(error.to_string(), "env(): attempted builtin failure");
    assert!(
        matches!(error, crate::resolver::ResolutionError::Knowledge(error)
        if matches!(*error, KnowledgeDiagnostic::Relative(KnowledgeError::Unavailable { source_line, .. }) if source_line.get() == Some(9)))
    );
}

#[test]
fn serialization_knowledge_diagnostic_keeps_its_terminal_cause_and_text() {
    let error = super::super::knowledge_serialization_error(KnowledgeError::Cycle {
        operation: "user function",
    });
    assert_eq!(
        error.to_string(),
        "cannot serialize at WASM provider boundary: unresolved access cycle while resolving user function"
    );
    assert!(std::error::Error::source(&error).is_none());
    assert!(
        matches!(error, crate::value::SerializationError::Knowledge { error, .. }
        if matches!(*error, KnowledgeDiagnostic::Absolute(KnowledgeError::Cycle { .. })))
    );
}

#[test]
fn pending_secret_is_not_a_known_value_and_keeps_protected_sites() {
    let value = secret(apply_wait());
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(matches!(&proof, Classification::Pending(_)));
    assert!(proof.clone().into_known().is_err());
    let sites = proof.sites().collect::<Vec<_>>();
    assert!(!sites.is_empty());
    assert!(
        sites
            .iter()
            .all(|site| site.phase() == Phase::Apply && site.scope == DiagnosticScope::Secret)
    );
    assert!(
        PlanResolved::try_from(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );

    let value = secret(V::resource_ref("private-binding", "token", vec![]));
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(matches!(&proof, Classification::Pending(_)));
    let unevaluated = secret(V::Deferred(DeferredValue::Interpolation(vec![])));
    assert!(
        matches!(PlanResolved::try_from(unevaluated.knowledge(&KnowledgeContext::plan(&ResolvedBindings::default())).unwrap()), Err(KnowledgeError::UnresolvedAtBoundary { sites })
        if sites.iter().all(|site| site.scope == DiagnosticScope::Secret))
    );
}

fn function_leaf() -> crate::parser::UserFunction<RelativeBindingName> {
    crate::parser::parse_file(
        "fn leaf() { 1 }",
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .user_functions
    .shift_remove("leaf")
    .unwrap()
}
#[test]
fn function_body_graph_is_classified_once_per_call() {
    use std::sync::Arc;
    type Parsed = Value<ParserPlaceholder, RelativeBindingName>;
    let template = function_leaf();
    let mut leaf = template.clone();
    leaf.body.value = Parsed::resource_ref("producer", "identifier", vec![]);
    let mut function = Arc::new(leaf);
    for level in 0..20 {
        let call = Parsed::Deferred(DeferredValue::ResolvedFunctionCall {
            function: function.clone(),
            functions: Arc::new(indexmap::IndexMap::new()),
            args: vec![],
            source_line: 1.into(),
        });
        let mut next = template.clone();
        next.name = format!("layer{level}");
        next.body.value = Parsed::Concrete(ConcreteValue::List(vec![call.clone(), call]));
        function = Arc::new(next);
    }
    let value = Parsed::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions: Arc::new(indexmap::IndexMap::new()),
        args: vec![],
        source_line: 1.into(),
    });
    with_classification_budget(400, || {
        assert!(
            value
                .knowledge(&KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .is_ok()
        );
    });
}
#[test]
fn access_step_does_not_classify_unselected_children() {
    let mut map = IndexMap::new();
    map.insert("selected".to_owned(), V::Concrete(ConcreteValue::Int(7)));
    for i in 0..1000 {
        map.insert(
            format!("unused{i}"),
            V::resource_ref("producer", "identifier", vec![]),
        );
    }
    with_classification_budget(8, || {
        let result = super::super::apply_value_path_segment(
            V::Concrete(ConcreteValue::Map(map)),
            super::super::PathSegment::Field {
                name: "selected".into(),
            },
            1,
            super::super::AccessMissPolicy::Error,
        )
        .unwrap()
        .into_value()
        .unwrap();
        assert_eq!(result, V::Concrete(ConcreteValue::Int(7)));
    });
}
#[test]
fn secret_propagates_classification_errors() {
    let function = std::sync::Arc::new(function_leaf());
    let id = std::sync::Arc::as_ptr(&function) as usize;
    let value = Value::<ParserPlaceholder, RelativeBindingName>::Deferred(DeferredValue::Secret(
        Box::new(Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions: std::sync::Arc::new(indexmap::IndexMap::new()),
            args: vec![],
            source_line: 1.into(),
        }))
        .into(),
    ));
    let mut position = Position::root();
    position.functions.push(id);
    assert!(matches!(
        classify(
            &value,
            &KnowledgeContext::<ParserPlaceholder, RelativeBindingName>::validation(),
            &mut position
        ),
        Err(KnowledgeError::Cycle { .. })
    ));
}

#[test]
fn access_classification_only_visits_the_selected_member() {
    let mut map = IndexMap::new();
    map.insert("selected".to_owned(), apply_wait());
    for i in 0..1000 {
        map.insert(
            format!("unused{i}"),
            V::resource_ref("producer", "identifier", vec![]),
        );
    }
    let access = super::super::deferred_access(
        V::Concrete(ConcreteValue::Map(map)),
        super::super::PathSegment::Field {
            name: "selected".into(),
        },
        1,
    );
    with_classification_budget(8, || {
        let proof = access
            .knowledge(&KnowledgeContext::plan(&ResolvedBindings::default()))
            .unwrap();
        assert_eq!(
            proof
                .sites()
                .map(|site| site.reason().clone())
                .collect::<Vec<_>>(),
            vec![
                PendingReason::Evaluation(EvaluationKind::Access),
                PendingReason::ReadBack
            ]
        );
        assert!(proof.sites().all(|site| site.phase() == Phase::Apply));
        let assert_path = |site: PendingSite<RelativeBindingName>| {
            assert!(
                site.inspect::<bool, Infallible>(|path, operands, _| Ok(path.segments()
                    == [crate::schema::LeafPathSegment::MapKey("selected".into())]
                    && operands == [OperandStep::AccessBase]))
                    .unwrap()
            );
        };
        assert_path(
            proof
                .sites()
                .find(|site| site.reason() == &PendingReason::ReadBack)
                .unwrap(),
        );
        let resolved = PlanResolved::try_from(proof).unwrap();
        assert_path(
            resolved
                .knowledge()
                .sites()
                .find(|site| site.reason() == &PendingReason::ReadBack)
                .unwrap(),
        );
    });
}

fn lexical_call(
    function: std::sync::Arc<crate::parser::UserFunction<RelativeBindingName>>,
) -> Value<ParserPlaceholder, RelativeBindingName> {
    Value::Deferred(DeferredValue::ResolvedFunctionCall {
        function,
        functions: std::sync::Arc::new(indexmap::IndexMap::new()),
        args: vec![],
        source_line: 1.into(),
    })
}

#[test]
fn shared_lexical_body_keeps_each_invocations_operand_timing() {
    use crate::schema::LeafPathSegment;
    use std::sync::Arc;
    type Parsed = Value<ParserPlaceholder, RelativeBindingName>;
    let mut parsed = crate::parser::parse_file(
        "fn identity(value: String): String { value }",
        std::path::Path::new("functions.crn"),
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let function = Arc::new(parsed.user_functions.shift_remove("identity").unwrap());
    let call = |argument: Parsed| {
        Parsed::Deferred(DeferredValue::ResolvedFunctionCall {
            function: Arc::clone(&function),
            functions: Arc::new(indexmap::IndexMap::new()),
            args: vec![argument.into()],
            source_line: 1.into(),
        })
    };
    let value = Parsed::Concrete(ConcreteValue::List(vec![
        call(Parsed::Concrete(ConcreteValue::String("ready".into()))),
        call(Parsed::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::PendingReRead(PendingReRead::not_found()),
        ))),
    ]));
    let bindings = ResolvedBindings::default();
    let knowledge = value.knowledge(&KnowledgeContext::plan(&bindings)).unwrap();
    let sites = knowledge.sites().collect::<Vec<_>>();
    let known = sites
        .iter()
        .filter(|site| site.path.segments() == [LeafPathSegment::ListIndex(0)])
        .collect::<Vec<_>>();
    let apply = sites
        .iter()
        .filter(|site| site.path.segments() == [LeafPathSegment::ListIndex(1)])
        .collect::<Vec<_>>();
    assert!(!known.is_empty() && !apply.is_empty());
    assert!(known.iter().all(|site| site.phase() == Phase::Plan));
    assert!(
        apply.iter().all(|site| site.phase() == Phase::Apply),
        "{apply:?}"
    );
    let Classification::ShapeKnown(shape) = &*knowledge else {
        panic!("partial list");
    };
    let kernel::ShapeNode::List(items) = shape.node.as_ref() else {
        panic!("list");
    };
    let analysis = |item: &Classification<'_, ParserPlaceholder, RelativeBindingName>| {
        let Classification::Pending(pending) = item else {
            panic!("call");
        };
        let kernel::PendingNode::Ordinary(inputs) = pending.node.as_ref() else {
            panic!("ordinary call");
        };
        inputs
            .iter()
            .find_map(|input| match &input.value {
                kernel::InputValue::Lexical(body) => Some(Arc::as_ptr(body) as usize),
                kernel::InputValue::Value(_) => None,
            })
            .unwrap()
    };
    assert_eq!(analysis(&items[0]), analysis(&items[1]));
    let Err(KnowledgeError::UnresolvedAtBoundary { sites }) = PlanResolved::try_from(knowledge)
    else {
        panic!("the ready unevaluated call must still be rejected");
    };
    assert!(
        sites
            .iter()
            .all(|site| site.path.segments() == [LeafPathSegment::ListIndex(0)])
    );
}

#[test]
fn apply_operation_preserves_independent_plan_operand_work() {
    let value: V = Value::Deferred(DeferredValue::Interpolation(vec![
        InterpolationPart::Expr(V::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::PendingReRead(PendingReRead::not_found()),
        ))),
        InterpolationPart::Expr(V::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![V::Concrete(ConcreteValue::String("ready".into())).into()],
            source_line: 1.into(),
        })),
    ]));
    let bindings = ResolvedBindings::default();
    let knowledge = value.knowledge(&KnowledgeContext::plan(&bindings)).unwrap();
    let Classification::Pending(operation) = &*knowledge else {
        panic!("interpolation");
    };
    assert_eq!(operation.phase(), Phase::Apply);
    assert!(
        matches!(PlanResolved::try_from(knowledge), Err(KnowledgeError::UnresolvedAtBoundary { sites }) if sites.len() == 1)
    );
}

#[test]
fn shared_body_sites_keep_each_call_position_and_protection() {
    let mut function = function_leaf();
    function.body.value = Value::resource_ref("private_source", "identifier", vec![]);
    let call = lexical_call(std::sync::Arc::new(function));
    let value = Value::Concrete(ConcreteValue::List(vec![call.clone(), call]));
    let proof = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let sites = proof
        .sites()
        .filter(|site| site.reason() == &PendingReason::ResourceSelection)
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2);
    for (index, site) in sites.iter().enumerate() {
        assert_eq!(
            site.path.segments(),
            [crate::schema::LeafPathSegment::ListIndex(index)]
        );
        assert_eq!(site.operand, [OperandStep::FunctionBody]);
    }
    let protected = Value::Deferred(DeferredValue::Secret(Box::new(value).into()));
    let protected = protected
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    let sites = protected.sites().collect::<Vec<_>>();
    assert_eq!(sites.len(), 4);
    assert!(
        sites
            .iter()
            .all(|site| site.scope == DiagnosticScope::Secret)
    );
    assert!(!format!("{protected:?} {sites:?}").contains("private_source"));
}

#[test]
fn shared_body_analysis_is_local_to_its_classification_context() {
    let mut function = function_leaf();
    function.body.value = Value::resource_ref("producer", "identifier", vec![]);
    let call = lexical_call(std::sync::Arc::new(function));
    let value = Value::Concrete(ConcreteValue::List(vec![call.clone(), call]));
    let validation = value
        .knowledge(&KnowledgeContext::validation())
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(validation.sites().all(|site| site.phase() == Phase::Plan));
    let bindings = ResolvedBindings::<ParserPlaceholder, RelativeBindingName>::default();
    let plan = value
        .knowledge(&KnowledgeContext::plan(&bindings))
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(plan.sites().all(|site| site.phase() == Phase::Apply));
    let resolved = PlanResolved::try_from(
        value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                &crate::binding_index::ResolvedBindings::default(),
            ))
            .unwrap(),
    )
    .unwrap();
    let sites = resolved
        .knowledge()
        .sites()
        .filter(|site| site.reason() == &PendingReason::ResourceSelection)
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2);
    assert_ne!(sites[0].path, sites[1].path);
}

#[test]
fn shared_failed_body_stays_guarded_only_in_its_coalesce_alternative() {
    type Parsed = Value<ParserPlaceholder, RelativeBindingName>;
    let mut function = function_leaf();
    function.body.value = Parsed::Deferred(DeferredValue::Unavailable {
        name: crate::builtins::BuiltinFunction::env,
        args: vec![],
        reason: crate::resource::UnavailableReason::from_builtin_error(
            "attempted body failure".into(),
        ),
        source_line: 1.into(),
    });
    let call = lexical_call(std::sync::Arc::new(function));
    let guarded = Parsed::Deferred(DeferredValue::Coalesce {
        value: Box::new(Parsed::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::PendingReRead(PendingReRead::not_found()),
        ))),
        default: Box::new(call.clone()),
    });
    assert!(
        PlanResolved::try_from(
            guarded
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        )
        .is_ok()
    );
    let active = Parsed::Concrete(ConcreteValue::List(vec![guarded, call]));
    assert!(matches!(
        PlanResolved::try_from(
            active
                .knowledge(&KnowledgeContext::plan(
                    &crate::binding_index::ResolvedBindings::default()
                ))
                .unwrap()
        ),
        Err(KnowledgeError::Unavailable { .. })
    ));
}

#[test]
fn shared_function_locals_keep_their_operand_and_apply_timing() {
    let mut function = function_leaf();
    function.local_lets.push((
        "captured".into(),
        Value::resource_ref("producer", "identifier", vec![]),
        1,
    ));
    let call = lexical_call(std::sync::Arc::new(function));
    let value = Value::Concrete(ConcreteValue::List(vec![call.clone(), call]));
    let bindings = ResolvedBindings::<ParserPlaceholder, RelativeBindingName>::default();
    let proof = value
        .knowledge(&KnowledgeContext::plan(&bindings))
        .map(|proof| proof.into_classification())
        .unwrap();
    assert!(!proof.permits_static_evaluation());
    assert!(proof.sites().all(|site| site.phase() == Phase::Apply));
    let resolved = PlanResolved::try_from(
        value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                &crate::binding_index::ResolvedBindings::default(),
            ))
            .unwrap(),
    )
    .unwrap();
    let sites = resolved
        .knowledge()
        .sites()
        .filter(|site| site.reason() == &PendingReason::ResourceSelection)
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2);
    for (index, site) in sites.iter().enumerate() {
        assert_eq!(
            site.path.segments(),
            [crate::schema::LeafPathSegment::ListIndex(index)]
        );
        assert_eq!(site.operand, [OperandStep::FunctionLocal(0)]);
    }
}
