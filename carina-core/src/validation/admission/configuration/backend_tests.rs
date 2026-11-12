use super::*;

fn directory(
    source: &str,
) -> Result<AdmittedConfiguration<kind::BackendAttribute>, BackendConfigError> {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("backend.crn");
    std::fs::write(&path, source).unwrap();
    std::fs::write(
        root.path().join("values.crn"),
        "let ordinary_value = 'value'\n",
    )
    .unwrap();
    let parsed = parser::parse_file(
        &std::fs::read_to_string(&path).unwrap(),
        &path,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    admit(parsed.backend.as_ref().unwrap())
}

#[test]
fn backend_config_directory_rejects_pending_and_secret_before_construction() {
    for source in [
        "backend local { path = missing.path }",
        "backend local { path = secret('do-not-show') }",
        "backend s3 { bucket = 'b' key = 'k' encrypt = missing.flag region = missing.region }",
    ] {
        let error = directory(source).unwrap_err();
        let BackendConfigError::Invalid(errors) = &error else {
            panic!("attribute errors: {error}")
        };
        assert!(!errors.errors().is_empty());
        assert!(!format!("{error:?} {error}").contains("do-not-show"));
        assert!(
            error.to_string().contains("Type mismatch"),
            "{source}: {error}"
        );
    }
}

#[test]
fn backend_receipts_keep_the_declaration_field_instance_and_revision() {
    let admitted = directory("backend s3 { bucket = 'b' key = 'k' }").unwrap();
    assert_eq!(admitted.fields.len(), admitted.attributes().len());
    for placed in &admitted.fields {
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
    let placed = admitted.fields[0].clone();
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
