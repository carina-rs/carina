use super::*;

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
