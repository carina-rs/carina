pub mod commands;
pub mod cursor;
pub mod display;
pub mod error;
#[cfg(any(test, feature = "test-utils"))]
pub mod fixture_plan;
pub mod kms;
pub mod moved_hint;
pub mod plan_image;
pub mod plan_summary;
pub mod signal;
pub mod wiring;

pub(crate) mod legacy_name_overrides;
pub(crate) mod module_walk;
#[cfg(test)]
pub(crate) mod provider_upstream_test_support;

#[cfg(test)]
mod lock_file_api_tests;
#[cfg(test)]
mod module_info_snapshot_tests;
#[cfg(test)]
mod module_list_tests;
#[cfg(test)]
mod plan_snapshot_tests;
#[cfg(test)]
mod tests;

use clap::ValueEnum;
use std::num::NonZeroUsize;

pub const DEFAULT_PARALLELISM: NonZeroUsize = NonZeroUsize::new(8).unwrap();

#[cfg(test)]
pub(crate) fn absolute_binding_for_test(
    name: impl Into<String>,
) -> carina_core::resource::AbsoluteBindingAddress {
    carina_core::resource::absolute_binding_address_for_test(name)
}

#[cfg(test)]
pub(crate) fn test_composition_call_path() -> carina_core::resource::CompositionCallPath {
    test_composition_call_path_in(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
}

#[cfg(test)]
pub(crate) fn test_composition_call_path_in(
    source_file: &std::path::Path,
) -> carina_core::resource::CompositionCallPath {
    let parsed = carina_core::parser::parse_file(
        "let fixture_module = use { source = './fixture_module' }\nfixture_module {}\n",
        source_file,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse composition test call");
    let call = parsed
        .module_calls
        .first()
        .expect("composition test call must exist");
    carina_core::resource::CompositionCallPath::direct(carina_core::resource::CompositionCall {
        id: call.id().clone(),
        module_name: call.module_name.clone(),
        binding: call.binding_name.clone().map(|binding| {
            carina_core::resource::absolute_binding_address_for_test(binding.into_string())
        }),
        instance: "fixture_module".to_string(),
        module_source: Some("./fixture_module".to_string()),
        module_directory: None,
        loop_origin: None,
    })
}

#[cfg(test)]
pub(crate) fn test_authored_composition_call_path()
-> carina_core::resource::CompositionCallPath<carina_core::resource::RelativeBindingName> {
    let source_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let parsed = carina_core::parser::parse_file(
        "let fixture_module = use { source = './fixture_module' }\nfixture_module {}\n",
        &source_file,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse authored composition test call");
    let call = parsed
        .module_calls
        .first()
        .expect("composition test call must exist");
    carina_core::resource::CompositionCallPath::direct(carina_core::resource::CompositionCall {
        id: call.id().clone(),
        module_name: call.module_name.clone(),
        binding: call.binding_name.clone(),
        instance: "fixture_module".to_string(),
        module_source: Some("./fixture_module".to_string()),
        module_directory: None,
        loop_origin: None,
    })
}

#[cfg(test)]
pub(crate) fn empty_lifted_saved_attrs() -> carina_core::provider::LiftedSavedAttrs {
    carina_core::provider::RawSavedAttrs::default()
        .lift(&carina_core::schema::SchemaRegistry::new())
}

#[cfg(test)]
pub(crate) fn managed_current_from_states(
    states: std::collections::HashMap<
        carina_core::resource::ResourceId,
        carina_core::resource::State,
    >,
) -> std::collections::HashMap<carina_core::resource::ResourceId, carina_core::resource::Current> {
    states
        .into_iter()
        .map(|(id, state)| (id, carina_core::resource::Current::from_state(state)))
        .collect()
}

#[cfg(test)]
pub(crate) fn plan_input_current_from_states(
    states: std::collections::HashMap<
        carina_core::resource::ResourceId,
        carina_core::resource::State,
    >,
    schemas: &carina_core::schema::SchemaRegistry,
    resources: &[carina_core::resource::Resource],
) -> std::collections::HashMap<
    carina_core::resource::ResourceId,
    carina_core::resource::PlanInputCurrent,
> {
    carina_core::resource::into_plan_input_current_map(
        managed_current_from_states(states),
        schemas,
        resources,
    )
}

#[cfg(test)]
pub(crate) fn fixture_applied_identifier(
    identifier: impl Into<String>,
) -> carina_core::provider::AppliedIdentifier {
    carina_core::provider::AppliedIdentifier::new(identifier)
        .expect("fixture provider identifier must be non-empty")
}

#[cfg(test)]
pub(crate) fn fixture_read_outcome(
    state: carina_core::resource::State,
) -> carina_core::provider::ReadOutcome {
    if !state.exists {
        return carina_core::provider::ReadOutcome::not_found(state.id);
    }
    let identifier = state
        .identifier
        .expect("existing read fixture must carry a provider identifier");
    carina_core::provider::ReadOutcome::found(
        carina_core::provider::AppliedState::new(state.id, identifier, state.attributes)
            .expect("existing read fixture identifier must be non-empty"),
    )
}

#[cfg(test)]
pub(crate) fn fixture_resource_state_metadata(
    row: carina_state::ResourceState,
    write_only_attributes: Option<Vec<String>>,
    partial_read: Option<Option<carina_core::resource::PartialReadMarker>>,
) -> carina_state::ResourceState {
    let mut encoded = serde_json::to_value(row).expect("fixture resource state serializes");
    let current = encoded
        .get_mut("current")
        .and_then(serde_json::Value::as_object_mut)
        .expect("fixture resource state has a tagged current instance");
    if let Some(write_only_attributes) = write_only_attributes {
        current.insert(
            "write_only_attributes".to_string(),
            serde_json::to_value(write_only_attributes)
                .expect("fixture write-only metadata serializes"),
        );
    }
    if let Some(partial_read) = partial_read {
        current.insert(
            "partial_read".to_string(),
            serde_json::to_value(partial_read).expect("fixture partial-read metadata serializes"),
        );
    }
    let state = serde_json::json!({
        "version": carina_state::StateFile::CURRENT_VERSION,
        "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
        "serial": 0,
        "lineage": "fixture-lineage",
        "carina_version": "test",
        "resources": [encoded],
        "exports": {},
    });
    let state = carina_state::state::check_and_migrate(&state.to_string())
        .expect("fixture state passes the public state loader")
        .into_state();
    state
        .resources()
        .first()
        .expect("fixture state contains one resource")
        .clone()
}

#[cfg(test)]
enum FixtureCreateResult {
    Untracked {
        error: carina_core::provider::ProviderError,
    },
    Pending {
        token: carina_core::provider::PendingToken,
        identifier: Option<carina_core::provider::AppliedIdentifier>,
        sent: carina_core::provider::Sent,
    },
    Observed {
        identifier: carina_core::provider::AppliedIdentifier,
        attributes: std::collections::HashMap<String, carina_core::resource::Value>,
        sent: carina_core::provider::Sent,
    },
}

#[cfg(test)]
struct FixtureCreateProvider {
    result: std::sync::Mutex<Option<FixtureCreateResult>>,
}

#[cfg(test)]
impl carina_core::provider::Provider for FixtureCreateProvider {
    fn name(&self) -> &str {
        "fixture-create"
    }

    fn read(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::ReadOutcome>,
    > {
        Box::pin(async {
            Err(carina_core::provider::ProviderError::internal(
                "read is not used by create fixtures",
            ))
        })
    }

    fn read_data_source(
        &self,
        _resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::resource::State>,
    > {
        Box::pin(async {
            Err(carina_core::provider::ProviderError::internal(
                "data-source read is not used by create fixtures",
            ))
        })
    }

    fn create(
        &self,
        _id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> carina_core::provider::BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        let id = _id.clone();
        let result = self
            .result
            .lock()
            .expect("fixture create result lock is not poisoned")
            .take()
            .expect("fixture create is dispatched once");
        Box::pin(async move {
            match result {
                FixtureCreateResult::Untracked { error } => Err(attempt.untracked(error)),
                FixtureCreateResult::Pending {
                    token,
                    identifier,
                    sent,
                } => {
                    let pending = match identifier {
                        Some(identifier) => attempt.issued(identifier, sent).pending(token, 1),
                        None => attempt.pending(token, sent, 1),
                    };
                    Err(
                        pending.preserve(carina_core::provider::ProviderError::timeout(
                            "fixture create remains pending",
                        )),
                    )
                }
                FixtureCreateResult::Observed {
                    identifier,
                    attributes,
                    sent,
                } => {
                    let state = carina_core::provider::AppliedState::from_identifier(
                        id,
                        identifier.clone(),
                        attributes,
                    );
                    Err(attempt.issued(identifier, sent).failed(
                        carina_core::provider::Observation::Read(
                            carina_core::provider::ReadOutcome::found(state),
                        ),
                        carina_core::provider::ProviderError::internal(
                            "fixture create reached the provider",
                        ),
                    ))
                }
            }
        })
    }

    fn resume_create(
        &self,
        request: carina_core::provider::ResumeCreateRequest,
    ) -> carina_core::provider::BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async move {
            let (resume, create_request) = request.into_parts();
            drop(create_request);
            Err(
                resume.preserve(carina_core::provider::ProviderError::internal(
                    "resume is not used by create fixtures",
                )),
            )
        })
    }

    fn update(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::UpdateOutcome>,
    > {
        Box::pin(async {
            Err(carina_core::provider::ProviderError::internal(
                "update is not used by create fixtures",
            ))
        })
    }

    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::DeleteRequest,
    ) -> carina_core::provider::BoxFuture<'_, carina_core::provider::ProviderResult<()>> {
        Box::pin(async {
            Err(carina_core::provider::ProviderError::internal(
                "delete is not used by create fixtures",
            ))
        })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> carina_core::provider::ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
fn block_on_ready<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};

    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("fixture future must complete synchronously"),
    }
}

#[cfg(test)]
fn dispatch_fixture_create(
    id: &carina_core::resource::ResourceId,
    result: FixtureCreateResult,
) -> carina_core::provider::CreateFailure {
    let mut resource = carina_core::resource::Resource::from_id(id.clone());
    let sent = match &result {
        FixtureCreateResult::Untracked { .. } => &carina_core::provider::Sent::AllRequestAttributes,
        FixtureCreateResult::Pending { sent, .. } | FixtureCreateResult::Observed { sent, .. } => {
            sent
        }
    };
    if let carina_core::provider::Sent::Only(names) = sent {
        for name in names {
            resource.set_attr(
                name.clone(),
                carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String("fixture".into()),
                ),
            );
        }
    }
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |type_checks| {
            let preparation = carina_core::executor::ProviderPreparationContext::new(
                &bindings,
                &module_gate,
                &[],
                &carina_core::provider::NoopNormalizer,
                &[],
                &schemas,
                type_checks,
            );
            let request = block_on_ready(carina_core::executor::prepare_create_request(
                resource,
                &preparation,
            ))
            .expect("fixture resource passes provider preparation");
            let provider = FixtureCreateProvider {
                result: std::sync::Mutex::new(Some(result)),
            };
            let (create, _cancellation) =
                carina_core::executor::dispatch_create(&provider, id, request);
            block_on_ready(create).expect_err("fixture create returns a classified failure")
        },
    )
}

#[cfg(test)]
pub(crate) fn fixture_in_flight_create(
    id: &carina_core::resource::ResourceId,
    token: impl Into<String>,
    identifier: Option<&str>,
    sent: carina_core::provider::Sent,
) -> carina_core::provider::InFlightCreate {
    let failure = dispatch_fixture_create(
        id,
        FixtureCreateResult::Pending {
            token: carina_core::provider::PendingToken::new(token),
            identifier: identifier.map(fixture_applied_identifier),
            sent,
        },
    );
    match failure.into_kind() {
        carina_core::provider::CreateFailureKind::InFlight {
            pending,
            error: _error,
        } => pending,
        carina_core::provider::CreateFailureKind::Created {
            created: _created,
            error,
        } => panic!("pending fixture was classified created: {error}"),
        carina_core::provider::CreateFailureKind::Untracked(error) => {
            panic!("pending fixture was classified untracked: {error}")
        }
        carina_core::provider::CreateFailureKind::OutcomeUnknown { error, .. } => {
            panic!("pending fixture was classified outcome unknown: {error}")
        }
    }
}

#[cfg(test)]
pub(crate) fn fixture_repend_in_flight_create(
    pending: carina_core::provider::InFlightCreate,
    token: impl Into<String>,
) -> carina_core::provider::InFlightCreate {
    let id = pending.id().clone();
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let module_gate = carina_core::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let schemas = carina_core::schema::SchemaRegistry::new();
    carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |type_checks| {
            let preparation = carina_core::executor::ProviderPreparationContext::new(
                &bindings,
                &module_gate,
                &[],
                &carina_core::provider::NoopNormalizer,
                &[],
                &schemas,
                type_checks,
            );
            let request = block_on_ready(carina_core::executor::prepare_create_request(
                carina_core::resource::Resource::from_id(id),
                &preparation,
            ))
            .expect("resume fixture resource passes provider preparation");
            let request = carina_core::provider::ResumeCreateRequest::for_test(pending, request)
                .expect("resume fixture route agrees with its checked request");
            let (resume, _request) = request.into_parts();
            let token = carina_core::provider::PendingToken::new(token);
            let repended = match resume {
                carina_core::provider::Resume::Unissued(attempt) => attempt.repend(token),
                carina_core::provider::Resume::Issued(resumed) => resumed.repend(token),
            };
            let failure = repended.preserve(carina_core::provider::ProviderError::timeout(
                "fixture create remains in progress",
            ));
            match failure.into_kind() {
                carina_core::provider::ResumeFailureKind::InFlight {
                    pending,
                    error: _error,
                } => pending,
                carina_core::provider::ResumeFailureKind::Created {
                    created: _created,
                    error,
                } => panic!("re-pend fixture was classified created: {error}"),
                carina_core::provider::ResumeFailureKind::Ended(error) => {
                    panic!("re-pend fixture was classified ended: {error}")
                }
            }
        },
    )
}

#[cfg(test)]
pub(crate) fn fixture_observed_create(
    id: &carina_core::resource::ResourceId,
    attributes: std::collections::HashMap<String, carina_core::resource::Value>,
    sent: carina_core::provider::Sent,
) -> carina_core::provider::ObservedCreate {
    let failure = dispatch_fixture_create(
        id,
        FixtureCreateResult::Observed {
            identifier: fixture_applied_identifier("created-id"),
            attributes,
            sent,
        },
    );
    match failure.into_kind() {
        carina_core::provider::CreateFailureKind::Created {
            created,
            error: _error,
        } => created,
        carina_core::provider::CreateFailureKind::InFlight {
            pending: _pending,
            error,
        } => panic!("observed fixture remained pending: {error}"),
        carina_core::provider::CreateFailureKind::Untracked(error) => {
            panic!("observed fixture was classified untracked: {error}")
        }
        carina_core::provider::CreateFailureKind::OutcomeUnknown { error, .. } => {
            panic!("observed fixture was classified outcome unknown: {error}")
        }
    }
}

/// Controls how much detail is shown in plan output (CLI-facing enum with clap support).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DetailLevel {
    /// Show all attributes: user-specified, defaults, read-only, and unchanged (dimmed)
    Full,
    /// Show only attributes explicitly specified in .crn file
    Explicit,
    /// Show resource names only (no attributes)
    None,
}

impl DetailLevel {
    /// Convert to the core `DetailLevel` enum used by `build_detail_rows`.
    pub fn to_core(self) -> carina_core::detail_rows::DetailLevel {
        match self {
            DetailLevel::Full => carina_core::detail_rows::DetailLevel::Full,
            DetailLevel::Explicit => carina_core::detail_rows::DetailLevel::Explicit,
            DetailLevel::None => carina_core::detail_rows::DetailLevel::NamesOnly,
        }
    }
}
