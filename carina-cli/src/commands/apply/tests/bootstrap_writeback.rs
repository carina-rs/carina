use super::*;
use carina_core::executor::dispatch_create_for_writeback;
use carina_core::provider::HostCreateFailureKind;

async fn assert_bootstrap_transport_writeback(
    behavior: BootstrapCreateBehavior,
    position: BootstrapSecretPosition,
) {
    let plaintext = ["bootstrap", "condition", "fixture"].join("-");
    let attribute = match behavior {
        BootstrapCreateBehavior::IdentifierReported => "observed",
        _ => "tags_secret",
    };
    let mut resource = Resource::with_provider("aws", "s3.Bucket", "state_bucket", None)
        .with_attribute("bucket", string_value("state-bucket"))
        .with_attribute(
            attribute,
            Value::Deferred(DeferredValue::Secret(
                Box::new(string_value(&plaintext)).into(),
            )),
        );
    if matches!(behavior, BootstrapCreateBehavior::Success) {
        resource =
            resource.with_attribute(position.attribute(), position.desired_value(&plaintext));
    }
    let (schemas, comparison_provider) = carina_core::provider::comparison_fixture_for_test(
        &bootstrap_bucket_schemas(),
        std::iter::once(&resource.id),
    );
    let provider = BootstrapRecordingProvider {
        create_calls: AtomicUsize::new(0).into(),
        behavior,
    };
    let operation_provider = crate::provider_text_fixtures::boundary_with_components(
        Box::new(provider.clone()),
        Box::new(NoopNormalizer),
        &schemas,
    );
    let (_temp, backend) = bootstrap_test_backend();
    let provider_context = carina_core::parser::LoadedProviderContext::empty_for_test();
    let provider_configs = empty_resolved_provider_configs();
    let mut parsed = resolve_file_for_test(carina_core::parser::InferredFile::default());
    parsed.resources.push(resource.clone()); // allow: direct — fixture test inspection
    let context = BootstrapCreateContext {
        provider_context: &provider_context,
        backend: backend.as_ref(),
        parsed: &parsed,
        provider_configs: &provider_configs,
        normalizer: &NoopNormalizer,
        schemas: &schemas,
    };
    let row = crate::wiring::settle_type_checks(
        carina_core::validation::deferred_types::TypeCheckSession::run_async(
            &provider_context,
            async |session| {
                let permit = prepare_bootstrap_creation(
                    crate::output_style::ColorMode::Plain,
                    &resource.id,
                    &context,
                    session,
                )
                .await
                .unwrap_or_else(|_| panic!("bootstrap input preparation must succeed"));
                // Deliberately pass the provider's stripped transport value to every row builder.
                let transport = permit.request().resource().as_inner().clone();
                assert!(
                    !transport
                        .attributes
                        .values()
                        .any(carina_core::value::contains_secret),
                    "transport must contain no Secret wrappers"
                );
                assert!(
                    matches!(transport.get_attr(attribute),
                        Some(Value::Concrete(ConcreteValue::String(value)))
                            if value.expose_plaintext() == plaintext),
                    "transport must retain the stripped fixture value"
                );
                if matches!(behavior, BootstrapCreateBehavior::Success) {
                    let nested = transport
                        .get_attr(position.attribute())
                        .expect("transport must retain the nested attribute");
                    let fields = match (position, nested) {
                        (
                            BootstrapSecretPosition::ListStructField,
                            Value::Concrete(ConcreteValue::List(elements)),
                        ) => elements.first(),
                        _ => Some(nested),
                    };
                    assert!(
                        matches!(fields,
                            Some(Value::Concrete(ConcreteValue::Map(fields)))
                                if matches!(fields.get("k"),
                                    Some(Value::Concrete(ConcreteValue::String(value)))
                                        if value.expose_plaintext() == plaintext)),
                        "transport must retain the stripped nested fixture value"
                    );
                }
                let comparison = comparison_provider
                    .comparison_for(&schemas, &transport.id)
                    .unwrap_or_else(|_| panic!("fixture comparison must be available"));
                let schema = Some(comparison.owner_schema().schema().as_schema());
                let (create, cancellation) =
                    dispatch_create_for_writeback(&operation_provider, permit);
                let outcome = create.await;
                drop(cancellation);
                let row = comparison
                    .context()
                    .with_observation(|comparison| match (behavior, outcome) {
                        (
                            BootstrapCreateBehavior::Success
                            | BootstrapCreateBehavior::PartialSuccess,
                            Ok(carina_core::executor::CreateWriteback {
                                identifier,
                                state,
                                request: accepted,
                                diagnostic,
                                ..
                            }),
                        ) => {
                            assert_eq!(identifier.as_str(), "state-bucket");
                            assert_eq!(state.identifier.as_deref(), Some(identifier.as_str()));
                            assert_eq!(
                                diagnostic.is_some(),
                                matches!(behavior, BootstrapCreateBehavior::PartialSuccess)
                            );
                            ResourceState::from_provider_state_for_resource_and_schema(
                                &transport,
                                &state,
                                None,
                                schema,
                                RowBasis::Applied(&accepted),
                                None,
                                comparison,
                            )
                        }
                        (BootstrapCreateBehavior::IdentifierReported, Err(failure)) => {
                            let HostCreateFailureKind::IdentifierReported { identified, .. } =
                                failure.kind()
                            else {
                                panic!("fixture must retain the identified failure");
                            };
                            ResourceState::from_observation(
                                &transport, identified, None, schema, None, comparison,
                            )
                        }
                        (BootstrapCreateBehavior::InFlight, Err(failure)) => {
                            let HostCreateFailureKind::InFlight { pending, .. } = failure.kind()
                            else {
                                panic!("fixture must retain the fresh continuation");
                            };
                            assert!(
                                matches!(
                                    pending.persistence(),
                                    carina_core::provider::InFlightPersistence::Fresh(_)
                                ),
                                "fixture must exercise fresh continuation writeback"
                            );
                            ResourceState::from_in_flight(
                                &transport, pending, None, schema, None, comparison,
                            )
                        }
                        _ => panic!("fixture must produce the selected writeback result"),
                    })
                    .unwrap_or_else(|_| panic!("fixture comparison must succeed"))
                    .unwrap_or_else(|_| panic!("transport row encoding must succeed"));
                Ok::<_, AppError>(row)
            },
        )
        .await,
        AppError::Validation,
    )
    .unwrap_or_else(|_| panic!("bootstrap type checks must succeed"));
    assert_eq!(provider.create_calls.load(Ordering::SeqCst), 1);
    let encoded =
        serde_json::to_string(&row).unwrap_or_else(|_| panic!("transport row must serialize"));
    assert!(
        !encoded.contains(&plaintext),
        "transport writeback must not persist the fixture value"
    );
    let row: serde_json::Value = serde_json::from_str(&encoded)
        .unwrap_or_else(|_| panic!("transport row must be valid JSON"));
    let attributes = match behavior {
        BootstrapCreateBehavior::InFlight => &row["current"]["envelope"]["secrets"],
        _ => &row["current"]["attributes"],
    };
    let marker = &attributes[attribute];
    assert_eq!(marker.as_object().map(serde_json::Map::len), Some(1));
    let digest = marker["$secret"]
        .as_str()
        .expect("transport row must retain a secret digest");
    let hash = carina_core::resource::RedactedSecretHash::from_encoded_digest(digest)
        .expect("transport row must retain a valid digest");
    assert!(
        hash.verify(serde_json::to_vec(&plaintext).expect("fixture must serialize")),
        "transport row digest must verify the fixture value"
    );
    if matches!(behavior, BootstrapCreateBehavior::Success) {
        let fields = position.encoded_fields(&attributes[position.attribute()]);
        let marker = &fields["k"];
        assert_eq!(marker.as_object().map(serde_json::Map::len), Some(1));
        let digest = marker["$secret"]
            .as_str()
            .expect("transport row must retain a nested secret digest");
        let hash = carina_core::resource::RedactedSecretHash::from_encoded_digest(digest)
            .expect("transport row must retain a valid nested digest");
        assert!(
            hash.verify(serde_json::to_vec(&plaintext).expect("fixture must serialize")),
            "transport row nested digest must verify the fixture value"
        );
        assert!(
            fields["public"].as_str() == Some("p"),
            "transport row public sibling must stay plain"
        );
    }
}

#[tokio::test]
async fn bootstrap_transport_applied_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::Success,
        BootstrapSecretPosition::MapEntry,
    )
    .await;
}

#[tokio::test]
async fn bootstrap_transport_partial_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::PartialSuccess,
        BootstrapSecretPosition::MapEntry,
    )
    .await;
}

#[tokio::test]
async fn bootstrap_transport_observation_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::IdentifierReported,
        BootstrapSecretPosition::MapEntry,
    )
    .await;
}

#[tokio::test]
async fn bootstrap_transport_in_flight_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::InFlight,
        BootstrapSecretPosition::MapEntry,
    )
    .await;
}

#[tokio::test]
async fn bootstrap_transport_struct_field_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::Success,
        BootstrapSecretPosition::StructField,
    )
    .await;
}

#[tokio::test]
async fn bootstrap_transport_list_struct_field_writeback_retains_secret_hash() {
    assert_bootstrap_transport_writeback(
        BootstrapCreateBehavior::Success,
        BootstrapSecretPosition::ListStructField,
    )
    .await;
}
