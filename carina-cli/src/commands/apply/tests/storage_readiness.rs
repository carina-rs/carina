use super::*;

struct StorageGuardFactory(Arc<AtomicUsize>);

impl ProviderFactory for StorageGuardFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "storage readiness guard provider"
    }

    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        carina_provider_mock::builtin_schemas()
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }

    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("unavailable storage must stop apply before provider construction")
    }

    fn create_normalizer(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("unavailable storage must stop apply before normalizer construction")
    }
}

#[tokio::test]
async fn storage_readiness_unavailable_stops_apply_before_state_write_or_provider_call() {
    let runtime = crate::test_support::TestCommandRuntime::new();
    let temp = tempfile::tempdir().unwrap();
    let project = ResolvedPath::new(temp.path()).unwrap();
    let provider_context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let source = "provider mock {}\nlet bucket = mock.test.resource { name = 'bucket' }\n";
    std::fs::write(temp.path().join("main.crn"), source).unwrap();
    let loaded =
        load_configuration_with_config(&project, &provider_context, &SchemaRegistry::new())
            .unwrap();
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let providers = ProviderLoad::for_test_with_provider_phase(
        &runtime,
        crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(StorageGuardFactory(
            Arc::clone(&provider_calls),
        ))]),
        &loaded.parsed.providers,
    );
    let bootstrap_unresolved = loaded.unresolved_parsed.clone();
    let validated_config = providers
        .validate_config_pair(
            loaded.parsed,
            loaded.unresolved_parsed,
            &project,
            false,
            &loaded.duplicate_declarations,
        )
        .unwrap();
    let message = "state storage requires an external repair";
    let backend = crate::test_support::UnavailableStorageBackend::new(message);

    // The command harness resolves an opaque verified backend itself. Exercise
    // its actual readiness match at the first boundary accepting StateBackend.
    let error = prepare_apply_storage(
        &runtime,
        &project,
        &provider_context,
        validated_config,
        bootstrap_unresolved,
        loaded.backend_file.as_ref(),
        &backend,
    )
    .await
    .expect_err("confirmed unavailability must abort apply");

    let AppError::Backend(carina_state::BackendError::Configuration(original)) = error else {
        panic!("unavailability must preserve the backend configuration error")
    };
    assert_eq!(original, message);
    assert_eq!(backend.readiness_calls(), 1);
    assert_eq!(provider_calls.load(Ordering::SeqCst), 0);
    assert!(!temp.path().join("carina.state.json").exists());
    assert_eq!(
        std::fs::read_to_string(temp.path().join("main.crn")).unwrap(),
        source
    );
}
