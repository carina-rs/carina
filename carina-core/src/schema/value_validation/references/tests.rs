use super::*;
use crate::parser::retained::SourceReference;

fn allow_reference(
    _: &CompiledAttributeType,
    _: &LeafPath,
    _: SourceReference<'_, RelativeBindingName>,
) -> Result<(), TypeError> {
    Ok(())
}

fn with_source(
    test: impl FnOnce(AuthoredCursor<'_>, Classification<'_, ParserPlaceholder, RelativeBindingName>),
) {
    with_files(
        "let source = mock.Source { output = 'abc' }\n",
        &[],
        |cursor, knowledge, _| test(cursor, knowledge),
    );
}

fn with_files(
    source: &str,
    files: &[(&str, &str)],
    test: impl FnOnce(
        AuthoredCursor<'_>,
        Classification<'_, ParserPlaceholder, RelativeBindingName>,
        &crate::binding_index::BindingIndex<'_, RelativeBindingName>,
    ),
) {
    let directory = tempfile::tempdir().unwrap();
    for (name, body) in [
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", source),
        ("main.crn", "mock.Sink { input = source.output }\n"),
    ] {
        std::fs::write(directory.path().join(name), body).unwrap();
    }
    for (name, body) in files {
        let path = directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
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
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(loaded.judged().file(), &schemas, &catalog);
    test(
        AuthoredCursor::authored(slot.cursor().unwrap()),
        knowledge,
        &bindings,
    );
}

#[test]
fn authored_reference_metadata_does_not_clone_a_reference_path() {
    with_source(|cursor, _| {
        assert_eq!(
            cursor_allocations::count(|| {
                for _ in 0..128 {
                    std::hint::black_box(cursor.source_reference());
                }
            }),
            0
        );
    });
}

#[test]
fn unsubstituted_reference_uses_no_reference_state_or_allocations() {
    with_files(
        "let source = mock.Source {}\n",
        &[],
        |cursor, knowledge, _| {
            assert!(!matches!(
                ClassifiedRef::from(&knowledge),
                ClassifiedRef::Known(_)
            ));
            let ordinary = |_: &CompiledAttributeType,
                            _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                            _: &LeafPath,
                            _: AuthoredCursor<'_>| Ok(Default::default());
            let policy = ReferencePolicy::new(
                &ordinary,
                ReferenceCheck {
                    schema_source: &|_| true,
                    check: &allow_reference,
                },
            );
            let coverage = Default::default();
            let mut callbacks = WalkCallbacks::new(AssignmentCallbacks {
                ordinary: &ordinary,
                authored: None,
            });
            callbacks.ordinary = OrdinaryCheck::References(&policy);
            let assignment = Some(AssignmentWalk {
                cursor,
                callbacks,
                coverage: &coverage,
            });
            let allocations = cursor_allocations::count(|| {
                assert!(
                    ReferenceInput::for_walk(
                        matches!(ClassifiedRef::from(&knowledge), ClassifiedRef::Known(_)),
                        assignment.as_ref()
                    )
                    .is_none()
                );
            });
            assert_eq!(allocations, 0);
            assert!(assignment.is_some());
        },
    );
}

#[test]
fn reference_member_trials_do_not_allocate_reference_state() {
    with_source(|cursor, _| {
        let ordinary = |_: &CompiledAttributeType,
                        _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                        _: &LeafPath,
                        _: AuthoredCursor<'_>| Ok(Default::default());
        let policy = ReferencePolicy::new(
            &ordinary,
            ReferenceCheck {
                schema_source: &|_| true,
                check: &allow_reference,
            },
        );
        let _scope = policy.enter(cursor.identity().unwrap());
        let coverage = Default::default();
        let mut callbacks = WalkCallbacks::new(AssignmentCallbacks {
            ordinary: &ordinary,
            authored: None,
        });
        callbacks.ordinary = OrdinaryCheck::References(&policy);
        let assignment = Some(AssignmentWalk {
            cursor,
            callbacks,
            coverage: &coverage,
        });
        let allocations = cursor_allocations::count(|| {
            assert!(ReferenceInput::for_walk(true, assignment.as_ref()).is_none());
        });
        assert_eq!(allocations, 0);
    });
}

#[test]
fn substituted_module_output_keeps_ordinary_memo_and_skips_reference_callback() {
    with_files(
        "let child = use { source = './child' }\nlet source = child {}\n",
        &[("child/exports.crn", "exports { output: String = 'abc' }\n")],
        |cursor, knowledge, bindings| {
            let attr = CompiledAttributeType::string();
            assert!(!matches!(
                ClassifiedRef::from(&knowledge),
                ClassifiedRef::Known(_)
            ));
            // Module outputs currently stay pending. Exercise the known-value
            // seam with that same non-schema authored source, independently of
            // whether a future loader can substitute its concrete export.
            let value = Value::<ParserPlaceholder, RelativeBindingName>::Concrete(
                ConcreteValue::String("abc".into()),
            );
            let knowledge = value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .unwrap()
                .into_classification();
            let calls = std::cell::Cell::new(0);
            let ordinary = |_: &CompiledAttributeType,
                            _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                            _: &LeafPath,
                            _: AuthoredCursor<'_>| Ok(Default::default());
            let reference =
                |_: &CompiledAttributeType,
                 _: &LeafPath,
                 _: SourceReference<'_, RelativeBindingName>| {
                    calls.set(calls.get() + 1);
                    Ok(())
                };
            let schema_source = |binding: &RelativeBindingName| {
                bindings.is_schema_attribute_source(binding.as_str())
            };
            let policy = ReferencePolicy::new(
                &ordinary,
                ReferenceCheck {
                    schema_source: &schema_source,
                    check: &reference,
                },
            );
            let coverage = Default::default();
            let mut callbacks = WalkCallbacks::new(AssignmentCallbacks {
                ordinary: &ordinary,
                authored: None,
            });
            callbacks.ordinary = OrdinaryCheck::References(&policy);
            let walk = AssignmentWalk {
                cursor,
                callbacks,
                coverage: &coverage,
            };
            let validator = ValueValidator::new(empty_defs_for_schema_walks(), None);
            for _ in 0..2 {
                assert!(
                    validator
                        .walk_with_assignment(
                            &attr,
                            (&knowledge).into(),
                            &FieldPath::new(),
                            FieldAncestry::WRITABLE,
                            &mut Vec::new(),
                            Some(walk.clone())
                        )
                        .accepts()
                );
            }
            assert_eq!(calls.get(), 0);
            assert_eq!(validator.checked.borrow().len(), 1);
            assert!(validator.reference_checked.0.borrow().is_empty());
        },
    );
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
                         _: &LeafPath,
                         _: SourceReference<'_, RelativeBindingName>| {
            Err(TypeError::ValidationFailed {
                message: "declared sink rejection".into(),
            })
        };
        let coverage = Default::default();
        let policy = ReferencePolicy::new(
            &ordinary,
            ReferenceCheck {
                schema_source: &|_| true,
                check: &reference,
            },
        );
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
                let scope = trial.then(|| policy.enter(identity));
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
                    checked.into_findings().into_located().is_empty(),
                    trial,
                    "member_first={member_first}, trial={trial}"
                );
            }
        }
    });
}

#[test]
fn reference_declaration_is_independent_of_union_trial_order() {
    for members in [
        vec![
            CompiledAttributeType::string(),
            CompiledAttributeType::int(),
        ],
        vec![
            CompiledAttributeType::int(),
            CompiledAttributeType::string(),
        ],
    ] {
        let union = CompiledAttributeType::union(members);
        with_source(|cursor, knowledge| {
            let calls = std::cell::Cell::new(0);
            let ordinary = |_: &CompiledAttributeType,
                            _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                            _: &LeafPath,
                            _: AuthoredCursor<'_>| Ok(Default::default());
            let reference =
                |attr: &CompiledAttributeType,
                 _: &LeafPath,
                 _: SourceReference<'_, RelativeBindingName>| {
                    assert!(std::ptr::eq(attr, &union));
                    calls.set(calls.get() + 1);
                    Ok(())
                };
            let findings = validate_authored_value_for_admission_with_references(
                &union,
                &knowledge,
                empty_defs_for_schema_walks(),
                FieldAncestry::WRITABLE,
                no_lookup(),
                cursor,
                AssignmentCallbacks {
                    ordinary: &ordinary,
                    authored: None,
                },
                ReferenceCheck {
                    schema_source: &|_| true,
                    check: &reference,
                },
                &mut Default::default(),
            );
            assert!(findings.into_located().is_empty());
            assert_eq!(calls.get(), 1);
        });
    }
}

#[test]
fn recovered_stream_survives_merges_without_changing_primary() {
    with_source(|cursor, _| {
        let attr = CompiledAttributeType::string();
        let error = source_mismatch(&cursor);
        let checked = PendingReferencedValue(Box::new(ReferenceResult {
            value: CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed),
            reference: Err(error),
            identity: cursor.identity().unwrap(),
            key: None,
        }))
        .finish(&FieldPath::new(), &ReferenceMemo::default())
        .into_checked();
        assert!(checked.accepts());
        let mut parent = CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed);
        parent.findings.authored.extend(checked.findings.authored);
        assert!(parent.accepts());
        let findings = parent.into_findings();
        assert_eq!(findings.symbolic_findings().len(), 1);
        assert_eq!(
            findings.symbolic_findings()[0].origin(),
            SymbolicFindingOrigin::RecoveredReference
        );
        assert_eq!(findings.into_located().len(), 1);
    });
}

#[test]
fn recovered_reference_memo_hits_allocate_only_what_the_ordinary_walk_does() {
    with_source(|cursor, knowledge| {
        let attr = CompiledAttributeType::string();
        let calls = std::cell::Cell::new(0);
        let ordinary = |_: &CompiledAttributeType,
                        _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                        _: &LeafPath,
                        _: AuthoredCursor<'_>| Ok(Default::default());
        let reference = |_: &CompiledAttributeType,
                         _: &LeafPath,
                         _: SourceReference<'_, RelativeBindingName>| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let policy = ReferencePolicy::new(
            &ordinary,
            ReferenceCheck {
                schema_source: &|_| true,
                check: &reference,
            },
        );
        let coverage = Default::default();
        let callbacks = WalkCallbacks::new(AssignmentCallbacks {
            ordinary: &ordinary,
            authored: None,
        });
        let direct = AssignmentWalk {
            cursor,
            callbacks: callbacks.clone(),
            coverage: &coverage,
        };
        let mut recovered = direct.clone();
        recovered.callbacks.ordinary = OrdinaryCheck::References(&policy);
        let validator = ValueValidator::new(empty_defs_for_schema_walks(), None);
        let check = |walk| {
            std::hint::black_box(validator.walk_with_assignment(
                &attr,
                (&knowledge).into(),
                &FieldPath::new(),
                FieldAncestry::WRITABLE,
                &mut Vec::new(),
                Some(walk),
            ));
        };
        check(direct.clone());
        check(recovered.clone());
        let expected = cursor_allocations::count(|| check(direct));
        let actual = cursor_allocations::count(|| check(recovered));
        assert_eq!(calls.get(), 1);
        assert_eq!(actual, expected);
    });
}

#[test]
fn reference_resolution_failures_keep_the_ordinary_error_once() {
    with_source(|cursor, knowledge| {
        for (attr, defs) in [
            (CompiledAttributeType::ref_("Missing"), BTreeMap::new()),
            (
                CompiledAttributeType::ref_("Cycle"),
                BTreeMap::from([("Cycle".into(), CompiledAttributeType::ref_("Cycle"))]),
            ),
        ] {
            let ordinary = |_: &CompiledAttributeType,
                            _: CheckValue<'_, ParserPlaceholder, RelativeBindingName>,
                            _: &LeafPath,
                            _: AuthoredCursor<'_>| Ok(Default::default());
            let calls = std::cell::Cell::new(0);
            let reference =
                |_: &CompiledAttributeType,
                 _: &LeafPath,
                 _: SourceReference<'_, RelativeBindingName>| {
                    calls.set(calls.get() + 1);
                    Ok(())
                };
            let lookup = no_lookup();
            let callbacks = AssignmentCallbacks {
                ordinary: &ordinary,
                authored: None,
            };
            let expected = validate_authored_value_for_admission(
                &attr,
                &knowledge,
                &defs,
                FieldAncestry::WRITABLE,
                lookup,
                cursor.clone(),
                callbacks,
                &mut Default::default(),
            )
            .into_located();
            assert_eq!(expected.len(), 1);
            let actual = validate_authored_value_for_admission_with_references(
                &attr,
                &knowledge,
                &defs,
                FieldAncestry::WRITABLE,
                lookup,
                cursor.clone(),
                callbacks,
                ReferenceCheck {
                    schema_source: &|_| true,
                    check: &reference,
                },
                &mut Default::default(),
            )
            .into_located();
            assert_eq!(actual, expected);
            assert_eq!(calls.get(), 0);
        }
    });
}

fn source_mismatch(cursor: &AuthoredCursor<'_>) -> TypeError {
    let reference = cursor.source_reference().unwrap().into_path();
    let TypeError::TypeMismatch { types } =
        TypeError::value_type_mismatch("mock.RecoveryProbe".into(), "String".into())
    else {
        unreachable!()
    };
    // The payload uses the shared constructor that judge_argument reaches
    // through error_with_origin.
    TypeError::Reference {
        error: Box::new(crate::resource::SecretDiagnosticError::Public(
            crate::validation::admission::AdmissionReferenceError::Computed(
                crate::validation::DeferredAccessTypeError::reference_mismatch(
                    crate::binding_index::RefTypeMismatch::direct(types),
                    reference.as_ref(),
                ),
            ),
        )),
        attribute: None,
    }
}

#[test]
fn equal_ordinary_and_recovered_errors_keep_distinct_transformation_origins() {
    with_source(|cursor, _| {
        let error = source_mismatch(&cursor);
        let path = FieldPath::new();
        let other = path.push_field("recovered");
        let mut ordinary = CheckedValue::rejected(&path, error.clone());
        ordinary
            .findings
            .additional_local_errors
            .push(error.clone());
        ordinary
            .findings
            .authored
            .recover(cursor.identity().unwrap(), &other, error);
        let wrapped = ordinary
            .clone()
            .wrap_local_errors(|error| error.at_struct_field("outer"));
        assert!(matches!(
            wrapped.local.error(),
            Some(TypeError::StructFieldError { .. })
        ));
        assert!(matches!(
            wrapped.findings.additional_local_errors[0],
            TypeError::StructFieldError { .. }
        ));
        let protected = ordinary.inside_secret().into_findings();
        use crate::resource::SecretDiagnosticError;
        for error in [
            protected.local_error().unwrap(),
            &protected.local[0].1,
            &protected.located[0].1,
            &protected.additional_local_errors[0],
        ] {
            assert!(
                matches!(error, TypeError::Reference { error, .. } if matches!(error.as_ref(), SecretDiagnosticError::InsideSecret))
            );
        }
        assert!(
            matches!(&protected.located[1].1, TypeError::Reference { error, .. } if matches!(error.as_ref(), SecretDiagnosticError::Public(_)))
        );
    });
}

#[test]
fn recovered_stream_deduplicates_authored_uses_after_ordinary_selection() {
    with_source(|cursor, _| {
        let attr = CompiledAttributeType::string();
        let mut checked = CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed);
        checked.findings.authored.recover(
            cursor.identity().unwrap(),
            &FieldPath::new(),
            source_mismatch(&cursor),
        );
        checked
            .findings
            .authored
            .extend(checked.findings.authored.clone());
        assert!(checked.accepts());
        let findings = checked.into_findings();
        assert_eq!(findings.symbolic_findings().len(), 1);
        assert_eq!(findings.clone().into_located().len(), 1);
        assert_eq!(findings.local_errors().count(), 1);
    });
}

#[test]
fn recovered_reference_never_rejects_an_ordinary_value_or_changes_its_fit() {
    with_source(|cursor, _| {
        let attr = CompiledAttributeType::string();
        let path = FieldPath::new();
        let error = source_mismatch(&cursor);
        let ordinary = CheckedValue::accepted(&attr, FitExactness::EXACT, FitKind::Failed);
        let checked = PendingReferencedValue(Box::new(ReferenceResult {
            value: ordinary,
            reference: Err(error.clone()),
            identity: cursor.identity().unwrap(),
            key: None,
        }))
        .finish(&path, &ReferenceMemo::default())
        .into_checked();
        assert!(
            checked.accepts(),
            "reference authority must not enter union fitting"
        );
        assert!(checked.local.error().is_none());
        assert!(checked.findings.local.is_empty());
        assert!(checked.findings.located.is_empty());
        let findings = checked.into_findings();
        assert_eq!(
            findings.clone().into_located(),
            vec![(path.canonical(), error.clone())]
        );
        assert_eq!(
            findings.local_errors().cloned().collect::<Vec<_>>(),
            vec![error]
        );
    });
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
