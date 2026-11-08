use super::*;
use carina_core::provider::*;

struct PartialReadProvider;

fn read_back_error(plaintext: &str) -> ProviderError {
    ProviderError::api_error_caused_by(
        "public incomplete read",
        std::io::Error::other(format!("invalid password {plaintext}; public={PUBLIC}")),
    )
}

impl Provider for PartialReadProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }

    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        unreachable!("partial read-back is reported by create/update")
    }

    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        unreachable!("fixture has no data source")
    }

    fn create(
        &self,
        _: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let reason = read_back_error(&fixture::password(request.resource().wire_attributes()));
        let outcome = attempt
            .issued(
                AppliedIdentifier::new("mock-id").unwrap(),
                Sent::AllRequestAttributes,
            )
            .partial(
                HashMap::new(),
                PartialReadDiagnostic::new(
                    carina_core::provider_text::ProviderDiagnostic::new(reason),
                    (vec!["password".into()])
                        .try_into()
                        .expect("nonempty partial attributes"),
                ),
            );
        Box::pin(async { Ok(outcome) })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        _: UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let plaintext = request
            .wire_patch()
            .iter()
            .find_map(|operation| match operation {
                WirePatchOp::Add { key, value }
                | WirePatchOp::Replace { key, value }
                | WirePatchOp::Reassert { key, value }
                    if key == "password" =>
                {
                    Some(fixture::wire_string(value))
                }
                _ => None,
            })
            .expect("checked update patch carries the secret");
        let reason = read_back_error(&plaintext);
        let outcome = UpdateOutcome::partial_success(
            AppliedState::from_identifier(id.clone(), identifier.clone(), HashMap::new()),
            carina_core::provider_text::ProviderDiagnostic::new(reason),
            (vec!["password".into()])
                .try_into()
                .expect("nonempty partial attributes"),
        );
        Box::pin(async { Ok(outcome) })
    }

    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        unreachable!("fixture has no pending create")
    }

    fn delete(&self, _: DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        unreachable!("fixture does not delete")
    }
}

async fn exercise_partial_read(update: bool) {
    let desired = resource(SECRET, true);
    let instance = fixture::text_fixtures::initialized_for_desired(
        Box::new(PartialReadProvider),
        std::slice::from_ref(desired.as_resource()),
    );
    let id = desired.id.clone();
    let mut plan = Plan::new();
    let mut current = HashMap::new();
    if update {
        let from = State::<carina_core::resource::NoPlaceholder>::existing(
            id.clone(),
            HashMap::from([(
                "password".into(),
                Value::Concrete(ConcreteValue::String("old".into())),
            )]),
        )
        .with_identifier("mock-id")
        .to_snapshot();
        current.insert(id.clone(), Current::from_snapshot(from.clone()));
        plan.add(Effect::Update {
            from: Box::new(from),
            identifier: AppliedIdentifier::new("mock-id").unwrap(),
            to: desired,
            changed_attributes: vec!["password".into()],
        });
    } else {
        plan.add(Effect::Create(desired));
    }
    let result = execute(instance.provider(), &plan, current).await;
    assert_eq!(result.failure_count(), 0);
    assert_eq!(result.partial_count, 1);
    let carina_core::executor::Applied::Complete {
        state: snapshot, ..
    } = &result.applied_states[&id]
    else {
        panic!("partial read-back must preserve the applied state")
    };
    let expected = format!("public incomplete read: invalid password {MASK}; public={PUBLIC}");
    let marker = snapshot.partial_read.as_ref().unwrap();
    assert_eq!(
        marker.pending().reason(),
        &carina_core::resource::ReReadReason::Incomplete {
            detail: expected.clone().into()
        },
    );
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::from_provider_state("test.resource", "probe", "mock", snapshot).unwrap(),
        )
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("carina.state.json");
    carina_state::backends::local_backend_for_test(path.clone())
        .write_state(&state)
        .await
        .unwrap();
    let text = std::fs::read_to_string(path).unwrap();
    assert_redacted(&text, SECRET);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        json["resources"][0]["current"]["partial_read"]["pending"]["reason"]["detail"],
        expected
    );

    let unknown = carina_core::resource::ApplyTimeUnknown::<
        carina_core::resource::AbsoluteBindingAddress,
    >::PendingReRead(marker.pending().clone());
    let displayed = carina_core::value::render_unknown(&unknown);
    assert!(displayed.contains(&expected), "{displayed}");
    assert_redacted(&displayed, SECRET);
    eprintln!("{displayed}");
}

#[test]
fn update_partial_provider_error_cause_is_redacted() {
    if let Some(stderr) = in_child(
        "update_partial::update_partial_provider_error_cause_is_redacted",
        || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(exercise_partial_read(true))
        },
    ) {
        assert!(stderr.contains("public incomplete read"), "{stderr}");
        assert!(stderr.contains(PUBLIC), "{stderr}");
        assert_redacted(&stderr, SECRET);
    }
}

#[test]
fn create_partial_provider_error_cause_is_redacted() {
    if let Some(stderr) = in_child(
        "update_partial::create_partial_provider_error_cause_is_redacted",
        || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(exercise_partial_read(false))
        },
    ) {
        assert!(stderr.contains("public incomplete read"), "{stderr}");
        assert!(stderr.contains(PUBLIC), "{stderr}");
        assert_redacted(&stderr, SECRET);
    }
}
