use super::*;

fn parse_evaluated_backend(
    source: &str,
    path: &std::path::Path,
    config: &ProviderContext,
) -> Result<parser::ParsedFile<parser::StrictBackend>, parser::ParseError> {
    let authored = parser::parse_file(source, path, config)?;
    parser::evaluate_file_backend::<parser::StrictBackend, _>(
        authored,
        config,
        None,
        crate::config_loader::strict_resolution_context(),
    )
}

#[test]
fn round17_retaining_loader_cannot_admit_failed_backend_without_recovery() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let directory = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        (
            "backend.crn",
            "backend s3 { bucket = backend_bucket() key = 'state.json' }",
        ),
        ("functions.crn", "fn backend_bucket(): String { true }"),
    ]);
    let parsed = crate::config_loader::load_configuration_for_metadata(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .unwrap()
    .parsed;
    assert!(
        parsed
            .admit_backend_unlocated()
            .is_some_and(|result| result.is_err()),
        "admission owns the retained evaluation failure"
    );
    let tree = parser::AnalysisTree::try_from_file(&parsed).unwrap();
    assert!(tree.admit_backend().is_some_and(|result| result.is_err()));
    assert!(
        matches!(tree.backend_attribute_value(&"bucket".into()), Some(parser::BackendAttributeValue::Failed(error)) if matches!(error.as_ref(), parser::ParseError::AtElement(_)))
    );
    let cause = match tree.backend_attribute_value(&"bucket".into()).unwrap() {
        parser::BackendAttributeValue::Failed(cause) => Arc::clone(cause),
        parser::BackendAttributeValue::Evaluated(_) => panic!("expected retained failure"),
    };
    let error = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        parsed
            .clone()
            .evaluate_backend_in_session(session)
            .expect_err("strict backend evaluation must fail")
    });
    let parser::AnalysisResolutionError::Diagnostic(ref finding) = error else {
        panic!("expected the new evaluation's source-owned cause")
    };
    assert_eq!(Some(finding.location().file().path()), cause.source_file());
    assert_eq!(error.to_string(), cause.to_string());
    assert!(admit(parsed.backend.as_ref().unwrap()).is_err());
}

#[test]
fn backend_admission_uses_the_issued_directory_verdict() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for declaration in [
        "arguments { name: String }",
        "attributes { name: String = 'example' }",
    ] {
        let source = format!("{declaration}\nbackend local {{ path = 1 typo = true }}\n");
        let parsed =
            parse_evaluated_backend(&source, std::path::Path::new("main.crn"), &context).unwrap();
        let parsed =
            crate::config_loader::RootDirectoryParse::from_parsed_source_for_test(parsed, &source);
        let tree = parser::AnalysisTree::try_from_file(&parsed).unwrap();
        assert!(
            matches!(tree.directory_verdict(), crate::config_loader::DirectoryVerdict::Root(root) if root.has_sole_backend()),
            "{declaration}"
        );
        let errors = tree.admit_backend().unwrap().unwrap_err();
        assert_eq!(errors.len(), 2, "{declaration}");
        for (finding, expected) in errors.into_iter().zip([
            "Invalid attribute 'path' for backend type 'local': Type mismatch: expected String, got Int",
            "Unknown attribute 'typo' for backend type 'local'",
        ]) {
            let LocatedBackendError::Attribute(finding) = finding else {
                panic!("expected an attribute failure")
            };
            assert_eq!(finding.detail().to_string(), expected);
            assert_eq!(finding.location().span().start_line(), 2);
        }

        let template =
            parse_evaluated_backend(declaration, std::path::Path::new("main.crn"), &context)
                .unwrap();
        let template = crate::config_loader::RootDirectoryParse::from_parsed_source_for_test(
            template,
            declaration,
        );
        let tree = parser::AnalysisTree::try_from_file(&template).unwrap();
        assert!(matches!(
            tree.directory_verdict(),
            crate::config_loader::DirectoryVerdict::ModuleTemplate(_)
        ));
        assert!(tree.admit_backend().is_none(), "{declaration}");
    }
}

#[test]
fn backend_admission_is_owned_by_its_analysis_tree() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let absent = parse_evaluated_backend(
        "let count = 1\n",
        std::path::Path::new("main.crn"),
        &context,
    )
    .unwrap();
    let tree = parser::AnalysisTree::try_from_file(&absent).unwrap();
    assert!(tree.admit_backend().is_none());

    let path = std::path::Path::new("backend.crn");
    let source = "backend local {\n  path = 1\n  typo = true\n}\n";
    let invalid = parse_evaluated_backend(source, path, &context).unwrap();
    let tree = parser::AnalysisTree::try_from_file(&invalid).unwrap();
    assert!(matches!(
        tree.directory_verdict(),
        crate::config_loader::DirectoryVerdict::Unknown(())
    ));
    assert!(tree.admit_backend().is_none());
    let invalid =
        crate::config_loader::RootDirectoryParse::from_parsed_source_for_test(invalid, source);
    let tree = parser::AnalysisTree::try_from_file(&invalid).unwrap();
    let errors = tree.admit_backend().unwrap().unwrap_err();
    assert_eq!(errors.len(), 2);
    for (index, (finding, expected)) in errors
        .into_iter()
        .zip([
            "Invalid attribute 'path' for backend type 'local': Type mismatch: expected String, got Int",
            "Unknown attribute 'typo' for backend type 'local'",
        ])
        .enumerate()
    {
        let LocatedBackendError::Attribute(finding) = finding else {
            panic!("expected an attribute failure")
        };
        assert_eq!(finding.detail().to_string(), expected);
        assert_eq!(finding.location().file().path(), path);
        assert_eq!(finding.location().span().start_line(), index + 2);
        assert_eq!(finding.location().span().start_column(), 3);
    }

    let path = std::path::Path::new("unsupported.crn");
    let source = "\nbackend gcs {}\n";
    let unsupported = parse_evaluated_backend(source, path, &context).unwrap();
    let unsupported =
        crate::config_loader::RootDirectoryParse::from_parsed_source_for_test(unsupported, source);
    let tree = parser::AnalysisTree::try_from_file(&unsupported).unwrap();
    let errors = tree.admit_backend().unwrap().unwrap_err();
    assert_eq!(errors.len(), 1);
    let LocatedBackendError::Unsupported(finding) = &errors[0] else {
        panic!("expected an unsupported backend")
    };
    assert_eq!(
        finding.detail().to_string(),
        "Unsupported backend type: gcs"
    );
    assert_eq!(finding.location().file().path(), path);
    assert_eq!(finding.location().span().start_line(), 2);
    assert_eq!(finding.location().span().start_column(), 1);

    let source = "backend local { path = 'state.json' }\n";
    let valid =
        parse_evaluated_backend(source, std::path::Path::new("valid.crn"), &context).unwrap();
    let valid =
        crate::config_loader::RootDirectoryParse::from_parsed_source_for_test(valid, source);
    let tree = parser::AnalysisTree::try_from_file(&valid).unwrap();
    assert_eq!(
        tree.admit_backend().unwrap().unwrap().backend_kind(),
        BackendKind::Local
    );
}

fn classification_errors() -> Vec<(KnowledgeError<AbsoluteBindingAddress>, &'static str)> {
    vec![
        (
            KnowledgeError::Cycle {
                operation: "user function",
            },
            "Validation failed: cycle while resolving user function",
        ),
        (
            KnowledgeError::InvalidStage {
                operation: "fixture classification",
            },
            "Validation failed: fixture classification is not valid at this resolution boundary",
        ),
        (
            KnowledgeError::UnavailableHistoricalSecret(
                serde_json::from_str::<crate::resource::UnavailableSecretError>(
                    r#"{"path":"saved.plan.provider.token"}"#,
                )
                .unwrap(),
            ),
            "Validation failed: secret value unavailable at saved.plan.provider.token",
        ),
    ]
}

#[test]
fn backend_classification_preserves_knowledge_category_and_text() {
    for (error, expected) in classification_errors() {
        let diagnostic = attribute_error(BackendKind::S3, &"bucket".into(), error.into());
        assert_eq!(
            diagnostic.to_string(),
            format!("Invalid attribute 'bucket' for backend type 's3': {expected}")
        );
        assert!(
            matches!(diagnostic, BackendAttributeError::InvalidAttribute { error, .. } if matches!(*error, TypeError::Knowledge { attribute: None, .. }))
        );
    }
}

#[test]
fn backend_receipts_keep_the_declaration_field_instance_and_revision() {
    let root = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        ("backend.crn", "backend s3 { bucket = 'b' key = 'k' }"),
        ("values.crn", "let ordinary = 'value'"),
    ]);
    let loaded = root.load(&ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let admitted = admit(loaded.parsed.backend.as_ref().unwrap()).unwrap();
    assert_eq!(admitted.placements().len(), admitted.attributes().len());
    for placed in admitted.placements().values() {
        assert_eq!(
            placed.value(),
            &admitted.attributes()[placed.inner.destination.contract.attribute.as_str()]
        );
    }
    let placed = admitted.placements().values().next().unwrap().clone();
    let destination = placed.inner.destination.clone();
    for (mismatch, different) in [
        (SlotMismatch::Declaration, 0),
        (SlotMismatch::Instance, 1),
        (SlotMismatch::Path, 2),
        (SlotMismatch::CatalogRevision, 3),
    ] {
        let mut wrong = placed.clone();
        match different {
            0 => wrong.inner.destination.identity.declaration.schema = DeclarationId::default(),
            1 => {
                wrong.inner.destination.identity.instance =
                    InstanceIdentity::Backend(BackendKind::Local)
            }
            2 => wrong.inner.destination.identity.path.push(99),
            _ => {
                wrong.inner.destination.identity.revision =
                    CatalogRevision(CatalogSnapshot::Backend(DeclarationId::default()))
            }
        }
        let mut slot = TypedSlot::new(destination.clone());
        slot.place(placed.clone()).unwrap();
        assert_eq!(slot.place(wrong), Err(mismatch));
        assert_eq!(slot.value(), Some(placed.value()));
    }
}

#[test]
fn round15_unlocated_backend_uses_the_same_ownership_gate() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for (sources, expected) in [
        (
            vec![
                ("backend.crn", "backend local { path = 1 }"),
                ("values.crn", "let ordinary = 'value'"),
            ],
            Some(1),
        ),
        (
            vec![
                ("backend.crn", "backend local { path = 1 }"),
                ("other.crn", "backend local {}"),
            ],
            None,
        ),
        (
            vec![
                ("declarations.crn", "arguments {}"),
                ("values.crn", "let ordinary = 'value'"),
            ],
            None,
        ),
    ] {
        let directory = crate::test_utils::configuration::ConfigurationDirectory::new(&sources);
        let parsed = crate::config_loader::parse_directory(
            &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
            &context,
        )
        .unwrap();
        let unlocated = parsed
            .admit_backend_unlocated()
            .map(|result| result.unwrap_err().len());
        let tree = parser::AnalysisTree::try_from_file(&parsed).unwrap();
        let located = tree.admit_backend().map(|result| result.unwrap_err().len());
        assert_eq!(unlocated, expected);
        assert_eq!(located, expected);
    }

    // The same issued root may retain a failed user call for diagnostics.
    // Neither source projection may turn it into an admitted backend.
    let directory = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        (
            "backend.crn",
            "backend s3 { bucket = backend_bucket() key = 'state.json' }",
        ),
        ("functions.crn", "fn backend_bucket(): String { true }"),
    ]);
    let parsed = crate::config_loader::parse_directory_with_overrides_and_diagnostics(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context,
        &Default::default(),
    )
    .unwrap()
    .parsed;
    assert!(
        parsed
            .admit_backend_unlocated()
            .is_some_and(|result| result.is_err())
    );
    assert!(
        parser::AnalysisTree::try_from_file(&parsed)
            .unwrap()
            .admit_backend()
            .is_some_and(|result| result.is_err())
    );
}

#[test]
fn round15_recovery_cannot_admit_failed_backend_values() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let source = "backend s3 { bucket = cidr_subnet('bad', 8, 1) key = cidr_subnet('bad', 8, 1) }";
    let directory = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        ("backend.crn", source),
        ("values.crn", "let ordinary = 'value'"),
    ]);
    let root = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let buffer = crate::config_loader::ParsedSourceBuffer::parse(
        source,
        &directory.path().join("backend.crn"),
        &context,
    )
    .unwrap();
    let snapshot =
        crate::config_loader::DirectoryBufferSnapshot::capture(&root, buffer, &Default::default())
            .unwrap();
    let schemas = crate::schema::SchemaRegistry::new();
    let configuration =
        crate::config_loader::load_buffer_configuration_in_directory(snapshot, &schemas, &context);
    let tree = parser::AnalysisTree::try_from_file(configuration.parsed()).unwrap();
    for key in ["bucket", "key"] {
        assert!(matches!(
            tree.backend_attribute_value(&key.into()),
            Some(parser::BackendAttributeValue::Failed(_))
        ));
    }
    assert!(
        tree.admit_backend().is_some_and(|result| result.is_err()),
        "admission must report failed evaluation without issuing readiness"
    );
}

#[test]
fn round19_failed_backend_preserves_error_source_chain() {
    let error = BackendAttributeError::EvaluationFailed {
        backend_type: "s3".into(),
        attribute: "bucket".into(),
        error: Arc::new(parser::ParseError::InvalidExpression {
            line: 1,
            message: "evaluation failed".into(),
        }),
    };
    assert!(std::error::Error::source(&error).is_some());
}

#[test]
fn round19_unlocated_backend_cause_is_reported_at_its_attribute() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let directory = crate::test_utils::configuration::ConfigurationDirectory::new(&[
        (
            "backend.crn",
            "backend s3 { bucket = cidr_subnet('bad', 8, 1) key = 'k' }",
        ),
        ("values.crn", "let other = 'state'"),
    ]);
    let loaded = crate::config_loader::load_configuration_for_metadata(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &context,
        &crate::schema::SchemaRegistry::new(),
    )
    .unwrap();
    let mut parsed = loaded.parsed.into_parsed();
    parsed.backend.as_mut().unwrap().attributes.insert(
        "bucket",
        parser::BackendAttributeValue::Failed(Arc::new(parser::ParseError::InvalidExpression {
            line: 1,
            message: "unlocated cause".into(),
        })),
    );
    let tree = parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let errors = tree
        .admit_backend()
        .expect("failed backend must be reported")
        .unwrap_err();
    assert_eq!(errors.len(), 1);
    let LocatedBackendError::Attribute(finding) = &errors[0] else {
        panic!("expected attribute error")
    };
    assert_eq!(
        finding.location().file().path().file_name().unwrap(),
        "backend.crn"
    );
    assert_eq!(finding.location().span().start_column(), 14);
    assert_eq!(
        finding.detail().to_string(),
        "Invalid expression at line 1: unlocated cause"
    );
}
