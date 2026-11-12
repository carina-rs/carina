use super::*;

fn with_source(
    test: impl FnOnce(AuthoredCursor<'_>, Classification<'_, ParserPlaceholder, RelativeBindingName>),
) {
    let directory = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        (
            "sources.crn",
            "let source = mock.Source { output = 'abc' }\n",
        ),
        ("main.crn", "mock.Sink { input = source.output }\n"),
    ] {
        std::fs::write(directory.path().join(name), body).unwrap();
    }
    let mut schemas = SchemaRegistry::new();
    for (kind, attribute) in [("Source", "output"), ("Sink", "input")] {
        schemas.insert(
            "mock",
            ResourceSchema::new(kind)
                .attribute(AttributeSchema::optional(attribute, AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
    }
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let parsed = crate::config_loader::parse_directory(&path, &catalog)
        .unwrap()
        .into_parsed();
    let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let tree = crate::parser::AnalysisTree::try_from_file(loaded.judged().file()).unwrap();
    let resource = tree
        .project_resources()
        .find(|resource| resource.id().resource_type == "Sink")
        .unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let knowledge = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    assert!(matches!(
        ClassifiedRef::from(&knowledge),
        ClassifiedRef::Known(_)
    ));
    test(AuthoredCursor::authored(slot.cursor().unwrap()), knowledge);
}

#[test]
fn reference_scope_does_not_reuse_member_trial_cache() {
    let union = CompiledAttributeType::union(vec![
        CompiledAttributeType::string(),
        CompiledAttributeType::int(),
    ]);
    let AttrTypeKind::Union(members) = &union.kind else {
        unreachable!()
    };
    let member = &members.as_raw_slice()[0];
    with_source(|cursor, knowledge| {
        let ordinary = |_: &CompiledAttributeType,
                        _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                        _: &LeafPath,
                        _: AuthoredCursor<'_>| Ok(Default::default());
        let reference = |_: &CompiledAttributeType,
                         _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                         _: &LeafPath,
                         _: AuthoredCursor<'_>| {
            Err(TypeError::ValidationFailed {
                message: "declared sink rejection".into(),
            })
        };
        let coverage = Default::default();
        let policy = ReferencePolicy::new(&ordinary, &reference);
        let mut callbacks = WalkCallbacks::new(AssignmentCallbacks {
            ordinary: &ordinary,
            authored: None,
        });
        callbacks.ordinary = OrdinaryCheck::References(&policy);
        let identity = cursor.source_reference().unwrap().identity;
        let base = AssignmentWalk {
            cursor,
            callbacks,
            coverage: &coverage,
        };
        for member_first in [false, true] {
            let validator = ValueValidator::new(empty_defs_for_schema_walks(), None);
            let checks = if member_first {
                [true, false]
            } else {
                [false, true]
            };
            for trial in checks {
                let scope = trial.then(|| policy.enter(identity, &union));
                let checked = validator.walk_with_assignment(
                    member,
                    (&knowledge).into(),
                    &FieldPath::new(),
                    FieldAncestry::WRITABLE,
                    &mut Vec::new(),
                    Some(base.clone()),
                );
                drop(scope);
                assert_eq!(
                    checked.accepts(),
                    trial,
                    "member_first={member_first}, trial={trial}"
                );
            }
        }
    });
}

#[test]
fn reference_declaration_is_independent_of_union_trial_order() {
    let union = CompiledAttributeType::union(vec![
        CompiledAttributeType::string(),
        CompiledAttributeType::int(),
    ]);
    let AttrTypeKind::Union(members) = &union.kind else {
        unreachable!()
    };
    let member = &members.as_raw_slice()[0];
    with_source(|cursor, _| {
        let ordinary = |_: &CompiledAttributeType,
                        _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                        _: &LeafPath,
                        _: AuthoredCursor<'_>| Ok(Default::default());
        let policy = ReferencePolicy::new(&ordinary, &ordinary);
        let identity = cursor.source_reference().unwrap().identity;
        for member_first in [false, true] {
            let root = policy.enter(identity, &union);
            let checks = if member_first {
                [member, &union]
            } else {
                [&union, member]
            };
            for attr in checks {
                let trial = policy.enter(identity, attr);
                assert_eq!(trial.scope.declaration, &union as *const _);
                assert!(!trial.owner);
            }
            assert!(root.owner);
        }
    });
}

#[test]
fn pending_reference_is_applied_before_any_findings_can_merge() {
    let attr = CompiledAttributeType::string();
    let error = TypeError::ValidationFailed {
        message: "reference rejection".into(),
    };
    let checked = CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed);
    let pending = PendingReferencedValue(Box::new(ReferenceResult {
        value: checked,
        reference: Some(Err(error)),
        key: None,
    }));
    let checked = pending.finish(&FieldPath::new(), &ReferenceMemo::default());
    assert!(!checked.accepts());
    let mut probe = AssignmentProbe::default();
    probe.extend(AssignmentProbe {
        ordinary: checked.findings.assignment.clone(),
        authored: Default::default(),
    });
    let mut parent = CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed);
    parent.extend_assignment(&FieldPath::new(), probe.ordinary);
    assert!(!parent.accepts());
    let findings = parent.into_findings();
    assert_eq!(findings.symbolic_findings().len(), 1);
    assert_eq!(
        findings.symbolic_findings()[0].origin(),
        SymbolicFindingOrigin::RecoveredReference
    );
    assert_eq!(findings.into_located().len(), 1);
}

#[test]
fn reference_policy_keeps_ordinary_callback_layout() {
    assert_eq!(
        std::mem::size_of::<OrdinaryCheck<'_, ParserPlaceholder, RelativeBindingName>>(),
        std::mem::size_of::<AssignmentCheck<'_, ParserPlaceholder, RelativeBindingName>>()
    );
}

#[test]
fn reference_policy_preserves_ordinary_walk_layout() {
    struct MainCallbacks<'a> {
        ordinary: AssignmentCheck<'a, ParserPlaceholder, RelativeBindingName>,
        authored: Option<AuthoredHook<'a, ParserPlaceholder, RelativeBindingName>>,
    }
    struct MainWalk<'a> {
        cursor: AuthoredCursor<'a>,
        callbacks: MainCallbacks<'a>,
        coverage: &'a std::cell::RefCell<WalkCoverage>,
    }
    use std::mem::{align_of, offset_of, size_of};
    type NewCallbacks<'a> = WalkCallbacks<'a, ParserPlaceholder, RelativeBindingName>;
    type NewWalk<'a> = AssignmentWalk<'a, ParserPlaceholder, RelativeBindingName>;
    assert_eq!(size_of::<MainCallbacks>(), size_of::<NewCallbacks>());
    assert_eq!(
        size_of::<Option<MainCallbacks>>(),
        size_of::<Option<NewCallbacks>>()
    );
    assert_eq!(align_of::<MainCallbacks>(), align_of::<NewCallbacks>());
    assert_eq!(
        offset_of!(MainCallbacks, ordinary),
        offset_of!(NewCallbacks, ordinary)
    );
    assert_eq!(
        offset_of!(MainCallbacks, authored),
        offset_of!(NewCallbacks, authored)
    );
    assert_eq!(size_of::<MainWalk>(), size_of::<NewWalk>());
    assert_eq!(size_of::<Option<MainWalk>>(), size_of::<Option<NewWalk>>());
    assert_eq!(offset_of!(MainWalk, cursor), offset_of!(NewWalk, cursor));
    assert_eq!(
        offset_of!(MainWalk, callbacks),
        offset_of!(NewWalk, callbacks)
    );
    assert_eq!(
        offset_of!(MainWalk, coverage),
        offset_of!(NewWalk, coverage)
    );
}
