use super::*;

#[test]
fn backend_admission_failure_retains_its_typed_cause_once() {
    let original = ResolutionError::from(KnowledgeError::<AbsoluteBindingAddress>::InvalidStage {
        operation: "backend fixture",
    });
    let diagnostic = attribute_error(BackendKind::Local, &"path".into(), original.clone().into());
    let BackendAttributeError::Admission { error, .. } = &diagnostic else {
        panic!("original non-constraint cause must survive")
    };
    assert_eq!(error.as_ref(), &original);
    assert_eq!(
        diagnostic
            .to_string()
            .matches(&original.to_string())
            .count(),
        1
    );
    assert!(
        std::error::Error::source(&diagnostic).is_none(),
        "terminal diagnostics render their cause without exposing it twice"
    );
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
