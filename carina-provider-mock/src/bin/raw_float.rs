//! Deliberately malformed raw WIT output for host admission tests.
//! The ordinary SDK provider cannot manufacture these value payloads.

#[cfg(target_arch = "wasm32")]
mod fixture {
    use carina_plugin_sdk::__private::provider as wit;
    use wit::*;

    struct RawFloatGuest;

    fn raw_float(selector: &str) -> f64 {
        match selector.rsplit('.').next().unwrap_or(selector) {
            "finite" => 1.0,
            "positive" => f64::INFINITY,
            "negative" => f64::NEG_INFINITY,
            _ => f64::NAN,
        }
    }

    impl Guest for RawFloatGuest {
        fn info() -> String {
            serde_json::to_string(&carina_plugin_sdk::types::ProviderInfoEnvelope {
                info: carina_plugin_sdk::types::ProviderInfo {
                    name: "mock".into(),
                    display_name: "Raw WIT float fixture".into(),
                    version: "0.0.0".into(),
                    capabilities: vec![],
                },
                protocol_version: carina_plugin_sdk::protocol::PROTOCOL_VERSION,
            })
            .unwrap()
        }

        fn schemas() -> String {
            r#"{"resources":[],"reference_forms":[],"assignability_edges":[]}"#.into()
        }
        fn provider_config_attribute_types() -> String {
            "{}".into()
        }
        fn provider_config_completions() -> String {
            "{}".into()
        }
        fn identity_attributes() -> Vec<String> {
            vec![]
        }
        fn validate_config(_attrs: Vec<(String, Value)>) -> Result<(), ProviderError> {
            Ok(())
        }
        fn initialize(_attrs: Vec<(String, Value)>) -> Result<InitializeResult, ProviderError> {
            Ok(InitializeResult {
                reference_scope: ReferenceScope::Unavailable,
            })
        }
        fn read(
            _id: ResourceId,
            identifier: String,
            _request: ReadRequest,
        ) -> Result<ReadOutcome, ProviderError> {
            Ok(ReadOutcome::Found(AppliedState {
                attributes: vec![("ratio".into(), Value::FloatVal(raw_float(&identifier)))],
                identifier,
            }))
        }
        fn read_data_source(res: ResourceDef) -> Result<State, ProviderError> {
            Ok(State {
                identifier: Some("raw-data".into()),
                exists: true,
                attributes: vec![("ratio".into(), Value::FloatVal(raw_float(&res.id.identity)))],
            })
        }
        fn normalize_desired(
            mut resources: Vec<DesiredResource>,
        ) -> Result<Vec<DesiredResource>, ProviderError> {
            // An earlier result is valid but must never be applied when the later one fails.
            for resource in &mut resources {
                resource.attributes.push((
                    "ratio".into(),
                    DesiredValue::FloatVal(raw_float(&resource.id.identity)),
                ));
            }
            Ok(resources)
        }
        fn normalize_state(
            mut states: Vec<(String, PersistedState)>,
        ) -> Result<Vec<(String, PersistedState)>, ProviderError> {
            for (key, state) in &mut states {
                state
                    .attributes
                    .push(("ratio".into(), PersistedValue::FloatVal(raw_float(key))));
            }
            Ok(states)
        }
        fn hydrate_read_state(
            states: Vec<(String, PersistedState)>,
            _saved: Vec<(String, Vec<(String, PersistedValue)>)>,
        ) -> Result<Vec<(String, PersistedState)>, ProviderError> {
            Self::normalize_state(states)
        }
        fn create(_id: ResourceId, _request: CreateRequest) -> Result<CreateStep, CreateVerdict> {
            Ok(CreateStep::Success(AppliedState {
                identifier: "raw-created".into(),
                attributes: vec![("ratio".into(), Value::FloatVal(f64::NAN))],
            }))
        }
        fn poll_create(
            _id: ResourceId,
            _request: CreateRequest,
            _pending: PendingCreate,
            _poll: PollRequest,
        ) -> Result<CreateStep, PollVerdict> {
            unreachable!()
        }
        fn resume_create(
            _id: ResourceId,
            _request: CreateRequest,
            _pending: ResumePending,
            _poll: PollRequest,
        ) -> Result<ResumeStep, ResumeVerdict> {
            unreachable!()
        }
        fn update(
            _id: ResourceId,
            identifier: String,
            _request: UpdateRequest,
        ) -> Result<UpdateOutcome, ProviderError> {
            Ok(UpdateOutcome::Success(AppliedPersistedState {
                identifier,
                attributes: vec![("ratio".into(), PersistedValue::FloatVal(f64::NAN))],
            }))
        }
        fn poll_update(
            _id: ResourceId,
            _identifier: String,
            _request: UpdateRequest,
            _pending: PendingUpdate,
            _poll: PollRequest,
        ) -> Result<UpdateOutcome, ProviderError> {
            unreachable!()
        }
        fn delete(
            _id: ResourceId,
            _identifier: String,
            _request: DeleteRequest,
        ) -> Result<DeleteOutcome, ProviderError> {
            unreachable!()
        }
        fn poll_delete(
            _id: ResourceId,
            _identifier: String,
            _request: DeleteRequest,
            _pending: PendingDelete,
        ) -> Result<DeleteOutcome, ProviderError> {
            unreachable!()
        }
        fn required_permissions(_id: ResourceId, _operation: PlanOp) -> Vec<String> {
            vec![]
        }
        fn satisfier_hint(_target_id: ResourceId, _path: Vec<String>) -> Vec<BindingPattern> {
            vec![]
        }
        fn validate_custom_type(_ty: TypeIdentity, _value: String) -> Result<(), ProviderError> {
            Ok(())
        }
        fn resolve_references(
            _requests: Vec<ReferenceRequest>,
        ) -> Result<Vec<Result<ReferenceOutcome, ReferenceError>>, ProviderError> {
            Ok(vec![])
        }
    }
    wit::export!(RawFloatGuest);
}

fn main() {}
