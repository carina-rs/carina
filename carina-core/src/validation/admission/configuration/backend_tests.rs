use super::*;

#[test]
fn backend_admission_is_owned_by_its_analysis_tree() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let absent = parser::parse_file(
        "let count = 1\n",
        std::path::Path::new("main.crn"),
        &context,
    )
    .unwrap();
    let tree = parser::AnalysisTree::try_from_file(&absent).unwrap();
    assert!(tree.admit_backend().is_none());

    let path = std::path::Path::new("backend.crn");
    let invalid = parser::parse_file(
        "backend local {\n  path = 1\n  typo = true\n}\n",
        path,
        &context,
    )
    .unwrap();
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
    let unsupported = parser::parse_file("\nbackend gcs {}\n", path, &context).unwrap();
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

    let valid = parser::parse_file(
        "backend local { path = 'state.json' }\n",
        std::path::Path::new("valid.crn"),
        &context,
    )
    .unwrap();
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
            &admitted.attributes()[placed
                .inner
                .destination
                .contract
                .as_ref()
                .unwrap()
                .attribute
                .as_str()]
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
