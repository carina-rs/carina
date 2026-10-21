//! Carina provider SDK for building external process providers.
//!
//! Implement the `CarinaProvider` trait and call `carina_plugin_sdk::run(provider)`
//! in your `main()` to start the JSON-RPC server loop.

pub use carina_provider_protocol as protocol;
pub use carina_provider_protocol::types;

#[cfg(any(target_arch = "wasm32", test))]
#[doc(hidden)]
pub mod wasm_guest;

#[cfg(target_arch = "wasm32")]
pub mod wasi_http;

// `wasi_http_body` is compiled for every target so its pure-Rust
// classification helpers can be unit-tested on the host. The only
// non-test caller (`wasi_http::make_request`) is wasm32-only, so on
// host builds the items are dead from the lib's point of view —
// silence the warning rather than guard the module on every fn.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod wasi_http_body;

/// Parse a ResourceId string (provider.resource_type.identity) into a ResourceId.
///
/// Format: "provider.service.type.identity" where provider is the first segment,
/// identity is the last segment, and resource_type is everything in between.
///
/// This is also used by the WASM guest SDK via `wasm_guest::parse_resource_id_string`.
pub fn parse_resource_id_string(key: &str) -> carina_provider_protocol::types::ResourceId {
    let parts: Vec<&str> = key.splitn(2, '.').collect();
    if parts.len() < 2 {
        return carina_provider_protocol::types::ResourceId {
            provider: String::new(),
            resource_type: String::new(),
            identity: key.to_string(),
        };
    }
    let provider = parts[0].to_string();
    let rest = parts[1];
    if let Some(dot_pos) = rest.rfind('.') {
        carina_provider_protocol::types::ResourceId {
            provider,
            resource_type: rest[..dot_pos].to_string(),
            identity: rest[dot_pos + 1..].to_string(),
        }
    } else {
        carina_provider_protocol::types::ResourceId {
            provider,
            resource_type: String::new(),
            identity: rest.to_string(),
        }
    }
}

use carina_provider_protocol::jsonrpc::{Notification, Request, Response};
use carina_provider_protocol::methods;
use carina_provider_protocol::types::{
    AttributeType, CompletionValue, CreateRequest, DeleteRequest, OperationConfig, PersistedState,
    PersistedValue, ProviderError, ProviderErrorKind, ProviderInfo, ReadRequest, Resource,
    ResourceId, ResourceSchema, State, TypeIdentity, UpdateRequest, Value,
};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::ops::ControlFlow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanOp {
    Create,
    Read,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BindingPattern {
    Exact(String),
    ForLoopChildren {
        base: String,
    },
    AttributeMatch {
        resource_type: String,
        attr: Vec<String>,
        from: Vec<String>,
    },
}

/// Provider-owned continuation for an operation that has not settled yet.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Pending<T> {
    pub token: T,
    pub poll_after_secs: u32,
}

/// SDK result of one create step.
#[derive(Debug)]
pub enum CreateOutcome<T> {
    Done(carina_provider_protocol::CreateOutcome),
    Pending(Pending<T>),
}

/// SDK result of one update step.
#[derive(Debug)]
pub enum UpdateOutcome<T> {
    Done(carina_provider_protocol::UpdateOutcome),
    Pending(Pending<T>),
}

/// SDK result of one delete step.
#[derive(Debug)]
pub enum DeleteOutcome<T> {
    Deleted,
    Pending(Pending<T>),
}

/// Token type for an operation kind that this provider can never defer.
#[derive(Debug)]
pub enum Never {}

impl serde::Serialize for Never {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match *self {}
    }
}

impl<'de> serde::Deserialize<'de> for Never {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(serde::de::Error::custom(
            "Never is uninhabited and cannot be deserialized",
        ))
    }
}

/// Trait that provider authors implement.
#[allow(clippy::result_large_err)]
pub trait CarinaProvider {
    type CreateToken: serde::Serialize + serde::de::DeserializeOwned;
    type UpdateToken: serde::Serialize + serde::de::DeserializeOwned;
    type DeleteToken: serde::Serialize + serde::de::DeserializeOwned;

    /// Return provider name and display name.
    fn info(&self) -> ProviderInfo;

    /// Return the list of optional capabilities this provider supports.
    /// Possible values: "normalize_desired", "normalize_state",
    /// "hydrate_read_state".
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// Return all resource schemas this provider supports.
    fn schemas(&self) -> Vec<ResourceSchema>;

    /// Return the types of the provider block's configuration attributes
    /// (e.g., `region`). The host uses these to validate attributes
    /// against their declared types *before* calling [`validate_config`].
    /// This keeps format validation on the host side so fixes in
    /// `carina-core` take effect without rebuilding provider binaries.
    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType>;

    /// Validate provider-specific configuration semantics not expressible
    /// in the attribute type schema. Host-side type validation has
    /// already run by the time this is called.
    fn validate_config(&self, attrs: &HashMap<String, Value>) -> Result<(), String>;

    /// Initialize the provider with configuration.
    /// Called once before any CRUD operations.
    fn initialize(&mut self, attrs: &HashMap<String, Value>) -> Result<(), String> {
        let _ = attrs;
        Ok(())
    }

    /// Read current state of a resource.
    ///
    /// `identifier` is `None` when no prior identifier exists (a fresh
    /// component, or a newly added resource on top of an existing
    /// component). In that case the provider MUST return
    /// [`State::not_found`] without contacting any external API.
    /// Encoding presence in the type — instead of using `""` as a
    /// sentinel — makes that contract enforceable at compile time
    /// (carina-rs/carina#2594 / #2596).
    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        request: ReadRequest,
    ) -> Result<State, ProviderError>;

    /// Read a data source resource.
    ///
    /// Unlike [`CarinaProvider::read`], this receives the full [`Resource`]
    /// so the plugin can see user-supplied input attributes (e.g. the
    /// `user_name` for `aws.identitystore.user`).
    ///
    /// Each provider must implement this explicitly. For zero-input data
    /// sources (e.g. `aws.sts.caller_identity`), the implementation can
    /// delegate to a state-only read against `resource.id`.
    fn read_data_source(&self, resource: &Resource) -> Result<State, ProviderError>;

    /// Create a new resource.
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError>;

    /// Continue a create operation previously returned as pending.
    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        pending: Pending<Self::CreateToken>,
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError>;

    /// Update an existing resource.
    ///
    /// Providers MUST NOT modify any attribute that is not represented in
    /// `request.patch.ops`. The patch is the sole source of truth for the
    /// update payload.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError>;

    /// Continue an update operation previously returned as pending.
    fn poll_update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
        pending: Pending<Self::UpdateToken>,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError>;

    /// Delete an existing resource.
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError>;

    /// Continue a delete operation previously returned as pending.
    fn poll_delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: DeleteRequest,
        pending: Pending<Self::DeleteToken>,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError>;

    /// Permissions this provider needs to perform `op` on `id`.
    /// Empty vec means the provider declares no permissions for this resource/op pair.
    fn required_permissions(
        &self,
        id: &ResourceId,
        op: PlanOp,
    ) -> Result<Vec<String>, ProviderError>;

    /// Binding-name patterns for resources that can satisfy a wait on `target_id.attr_path`.
    /// Empty vec means the provider declares no satisfier hint for this target attribute.
    fn satisfier_hint(
        &self,
        _target_id: &ResourceId,
        _attr_path: &[String],
    ) -> Result<Vec<BindingPattern>, ProviderError> {
        Ok(Vec::new())
    }

    /// Return provider config attribute completions.
    /// Key is attribute name (e.g., "region"), value is list of completion candidates.
    fn config_completions(&self) -> HashMap<String, Vec<CompletionValue>> {
        HashMap::new()
    }

    /// Return identity attribute names for anonymous resource ID computation.
    fn identity_attributes(&self) -> Vec<String> {
        vec![]
    }

    /// Validate a value against a provider-defined custom type.
    /// Returns `Ok(())` if the value is valid or the type is unknown.
    /// Returns `Err(message)` if the value is invalid for the given type.
    ///
    /// `identity` is the structured [`TypeIdentity`] of the type, so a
    /// provider matches on the discrete axes instead of splitting a
    /// flat name string.
    fn validate_custom_type(&self, _identity: &TypeIdentity, _value: &str) -> Result<(), String> {
        Ok(())
    }

    /// Normalize desired resources (optional).
    fn normalize_desired(&self, resources: Vec<Resource>) -> Result<Vec<Resource>, ProviderError> {
        Ok(resources)
    }

    /// Normalize read-back state (optional).
    fn normalize_state(
        &self,
        states: HashMap<String, PersistedState>,
    ) -> Result<HashMap<String, PersistedState>, ProviderError> {
        Ok(states)
    }

    /// Hydrate read state with saved attributes that APIs don't return.
    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, PersistedState>,
        saved_attrs: &HashMap<String, HashMap<String, PersistedValue>>,
    ) -> Result<(), ProviderError> {
        let _ = (states, saved_attrs);
        Ok(())
    }
}

/// Start the JSON-RPC server loop.
///
/// Reads JSON-RPC requests from stdin (one per line), dispatches to the
/// provider, and writes JSON-RPC responses to stdout (one per line).
///
/// Call this from `main()`:
/// ```ignore
/// fn main() {
///     carina_plugin_sdk::run(MyProvider::default());
/// }
/// ```
pub fn run(mut provider: impl CarinaProvider) {
    let operation_configs = operation_config_map(provider.schemas());
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    // Send ready notification
    let ready = Notification::ready();
    let ready_json = serde_json::to_string(&ready).expect("Failed to serialize ready");
    writeln!(writer, "{ready_json}").expect("Failed to write ready");
    writer.flush().expect("Failed to flush");

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — host closed stdin
            Ok(_) => {}
            Err(e) => {
                eprintln!("Failed to read stdin: {e}");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Failed to parse JSON-RPC request: {e}");
                continue;
            }
        };

        let response = dispatch(&mut provider, &operation_configs, &request);

        let resp_json = serde_json::to_string(&response).expect("Failed to serialize response");
        writeln!(writer, "{resp_json}").expect("Failed to write response");
        writer.flush().expect("Failed to flush");

        // Exit after shutdown
        if request.method == "shutdown" {
            break;
        }
    }
}

fn operation_config_map(schemas: Vec<ResourceSchema>) -> HashMap<String, OperationConfig> {
    schemas
        .into_iter()
        .map(|schema| {
            (
                schema.resource_type,
                schema.operation_config.unwrap_or_default(),
            )
        })
        .collect()
}

fn operation_budget(
    operation_configs: &HashMap<String, OperationConfig>,
    id: &ResourceId,
    kind: carina_provider_protocol::OperationKind,
) -> std::time::Duration {
    operation_configs.get(&id.resource_type).map_or(
        carina_provider_protocol::DEFAULT_OPERATION_BUDGET,
        |config| config.budget(kind),
    )
}

fn operation_timeout(id: &ResourceId, operation: &'static str) -> ProviderError {
    ProviderError {
        kind: ProviderErrorKind::Timeout,
        message: format!("provider {operation} operation exceeded its schema budget"),
        resource_id: Some(id.clone()),
        cause: None,
        provider_name: None,
        operation: Some(operation.to_string()),
        status: None,
        code: None,
        request_id: None,
    }
}

trait DrivenOutcome {
    type Output;
    type PendingState;

    fn into_control_flow(self) -> ControlFlow<Self::Output, Self::PendingState>;
    fn poll_after_secs(pending: &Self::PendingState) -> u32;
}

impl<T> DrivenOutcome for CreateOutcome<T> {
    type Output = carina_provider_protocol::CreateOutcome;
    type PendingState = Pending<T>;

    fn into_control_flow(self) -> ControlFlow<Self::Output, Self::PendingState> {
        match self {
            Self::Done(outcome) => ControlFlow::Break(outcome),
            Self::Pending(pending) => ControlFlow::Continue(pending),
        }
    }

    fn poll_after_secs(pending: &Self::PendingState) -> u32 {
        pending.poll_after_secs
    }
}

impl<T> DrivenOutcome for UpdateOutcome<T> {
    type Output = carina_provider_protocol::UpdateOutcome;
    type PendingState = Pending<T>;

    fn into_control_flow(self) -> ControlFlow<Self::Output, Self::PendingState> {
        match self {
            Self::Done(outcome) => ControlFlow::Break(outcome),
            Self::Pending(pending) => ControlFlow::Continue(pending),
        }
    }

    fn poll_after_secs(pending: &Self::PendingState) -> u32 {
        pending.poll_after_secs
    }
}

impl<T> DrivenOutcome for DeleteOutcome<T> {
    type Output = ();
    type PendingState = Pending<T>;

    fn into_control_flow(self) -> ControlFlow<Self::Output, Self::PendingState> {
        match self {
            Self::Deleted => ControlFlow::Break(()),
            Self::Pending(pending) => ControlFlow::Continue(pending),
        }
    }

    fn poll_after_secs(pending: &Self::PendingState) -> u32 {
        pending.poll_after_secs
    }
}

#[allow(clippy::result_large_err)]
fn drive_operation<Outcome, Initial, Poll>(
    id: &ResourceId,
    operation: &'static str,
    budget: std::time::Duration,
    initial: Initial,
    mut poll: Poll,
) -> Result<Outcome::Output, ProviderError>
where
    Outcome: DrivenOutcome,
    Initial: FnOnce() -> Result<Outcome, ProviderError>,
    Poll: FnMut(Outcome::PendingState) -> Result<Outcome, ProviderError>,
{
    let operation_started = std::time::Instant::now();
    let mut outcome = initial()?;
    loop {
        let pending = match outcome.into_control_flow() {
            ControlFlow::Break(output) => return Ok(output),
            ControlFlow::Continue(pending) => pending,
        };
        let remaining = budget.saturating_sub(operation_started.elapsed());
        match carina_provider_protocol::next_poll(Outcome::poll_after_secs(&pending), remaining) {
            carina_provider_protocol::NextPoll::OverBudget => {
                return Err(operation_timeout(id, operation));
            }
            carina_provider_protocol::NextPoll::Sleep(delay) => std::thread::sleep(delay),
        }
        outcome = poll(pending)?;
    }
}

#[allow(clippy::result_large_err)]
fn dispatch(
    provider: &mut impl CarinaProvider,
    operation_configs: &HashMap<String, OperationConfig>,
    request: &Request,
) -> Response {
    let id = request.id;

    match request.method.as_str() {
        "provider_info" => {
            let mut info = provider.info();
            info.capabilities = provider.capabilities();
            Response::success(id, methods::ProviderInfoResult { info })
        }

        "validate_config" => {
            let params: methods::ValidateConfigParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let error = provider.validate_config(&params.attributes).err();
            Response::success(id, methods::ValidateConfigResult { error })
        }

        "schemas" => {
            let schemas = provider.schemas();
            Response::success(id, methods::SchemasResult { schemas })
        }

        "initialize" => {
            let params: methods::InitializeParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.initialize(&params.attributes) {
                Ok(()) => Response::success(id, methods::InitializeResult { ok: true }),
                Err(e) => Response::error(id, -1, e),
            }
        }

        "read" => {
            let params: methods::ReadParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.read(&params.id, params.identifier.as_deref(), params.request) {
                Ok(state) => Response::success(id, methods::ReadResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "create" => {
            let params: methods::CreateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let budget = operation_budget(
                operation_configs,
                &params.id,
                carina_provider_protocol::OperationKind::Create,
            );
            match drive_operation(
                &params.id,
                "create",
                budget,
                || provider.create(&params.id, params.request.clone()),
                |pending| provider.poll_create(&params.id, params.request.clone(), pending),
            ) {
                Ok(outcome) => Response::success(id, methods::CreateResult { outcome }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "update" => {
            let params: methods::UpdateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let budget = operation_budget(
                operation_configs,
                &params.id,
                carina_provider_protocol::OperationKind::Update,
            );
            match drive_operation(
                &params.id,
                "update",
                budget,
                || provider.update(&params.id, &params.identifier, params.request.clone()),
                |pending| {
                    provider.poll_update(
                        &params.id,
                        &params.identifier,
                        params.request.clone(),
                        pending,
                    )
                },
            ) {
                Ok(outcome) => Response::success(id, methods::UpdateResult { outcome }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "delete" => {
            let params: methods::DeleteParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let budget = operation_budget(
                operation_configs,
                &params.id,
                carina_provider_protocol::OperationKind::Delete,
            );
            match drive_operation(
                &params.id,
                "delete",
                budget,
                || provider.delete(&params.id, &params.identifier, params.request.clone()),
                |pending| {
                    provider.poll_delete(
                        &params.id,
                        &params.identifier,
                        params.request.clone(),
                        pending,
                    )
                },
            ) {
                Ok(()) => Response::success(id, methods::DeleteResult { ok: true }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "normalize_desired" => {
            let params: methods::NormalizeDesiredParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.normalize_desired(params.resources) {
                Ok(resources) => {
                    Response::success(id, methods::NormalizeDesiredResult { resources })
                }
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "normalize_state" => {
            let params: methods::NormalizeStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.normalize_state(params.states) {
                Ok(states) => Response::success(id, methods::NormalizeStateResult { states }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "hydrate_read_state" => {
            let params: methods::HydrateReadStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let mut states = params.states;
            match provider.hydrate_read_state(&mut states, &params.saved_attrs) {
                Ok(()) => Response::success(id, methods::HydrateReadStateResult { states }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "shutdown" => Response::success(id, serde_json::json!({"ok": true})),

        _ => Response::error(id, -32601, format!("Unknown method: {}", request.method)),
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: &Option<serde_json::Value>,
) -> Result<T, String> {
    match params {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("Invalid params: {e}")),
        None => serde_json::from_value(serde_json::json!({}))
            .map_err(|e| format!("Missing params: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_token_decode_always_fails() {
        for encoded in ["null", "{}", "0", "\"token\""] {
            assert!(serde_json::from_str::<Never>(encoded).is_err());
        }
        assert!(
            serde_json::from_str::<Pending<Never>>(r#"{"token":null,"poll_after_secs":1}"#,)
                .is_err()
        );
    }

    #[derive(Default)]
    struct PendingDispatchProvider {
        create_polls: std::cell::Cell<usize>,
    }

    impl CarinaProvider for PendingDispatchProvider {
        type CreateToken = u8;
        type UpdateToken = Never;
        type DeleteToken = Never;

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "pending-dispatch".to_string(),
                display_name: "Pending Dispatch".to_string(),
                capabilities: Vec::new(),
                version: "0.0.0".to_string(),
            }
        }

        fn schemas(&self) -> Vec<ResourceSchema> {
            Vec::new()
        }

        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::new()
        }

        fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
            Ok(())
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> Result<State, ProviderError> {
            unreachable!("read is not used by this test")
        }

        fn read_data_source(&self, _resource: &Resource) -> Result<State, ProviderError> {
            unreachable!("read_data_source is not used by this test")
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
        ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
            Ok(CreateOutcome::Pending(Pending {
                token: 1,
                poll_after_secs: 0,
            }))
        }

        fn poll_create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            pending: Pending<Self::CreateToken>,
        ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
            assert_eq!(pending.token, 1);
            self.create_polls.set(self.create_polls.get() + 1);
            Ok(CreateOutcome::Done(protocol::CreateOutcome::Success {
                state: State {
                    id: id.clone(),
                    identifier: Some("created".to_string()),
                    attributes: HashMap::new(),
                    exists: true,
                },
            }))
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            unreachable!("update is not used by this test")
        }

        fn poll_update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
            pending: Pending<Self::UpdateToken>,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            match pending.token {}
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
            unreachable!("delete is not used by this test")
        }

        fn poll_delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
            pending: Pending<Self::DeleteToken>,
        ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
            match pending.token {}
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn json_rpc_dispatch_drives_pending_create_to_settled_wire_outcome() {
        let mut provider = PendingDispatchProvider::default();
        let operation_configs = operation_config_map(provider.schemas());
        let id = ResourceId {
            provider: "mock".to_string(),
            resource_type: "test.resource".to_string(),
            identity: "one".to_string(),
        };
        let request = Request::new(
            7,
            "create",
            methods::CreateParams {
                id: id.clone(),
                request: CreateRequest {
                    resource: Resource {
                        id,
                        attributes: HashMap::new(),
                        directives: protocol::Directives::default(),
                    },
                },
            },
        );

        let response = dispatch(&mut provider, &operation_configs, &request);

        assert!(response.error.is_none(), "{response:?}");
        let result: methods::CreateResult = serde_json::from_value(
            response
                .result
                .expect("successful response must have a result"),
        )
        .expect("JSON-RPC create result must retain the settled wire shape");
        assert!(matches!(
            result.outcome,
            protocol::CreateOutcome::Success { .. }
        ));
        assert_eq!(provider.create_polls.get(), 1);
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn json_rpc_operation_budget_includes_initial_call() {
        let id = ResourceId {
            provider: "mock".to_string(),
            resource_type: "test.resource".to_string(),
            identity: "slow-initial-call".to_string(),
        };
        let polls = std::cell::Cell::new(0);

        let error = drive_operation::<CreateOutcome<u8>, _, _>(
            &id,
            "create",
            std::time::Duration::from_millis(1),
            || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                Ok(CreateOutcome::Pending(Pending {
                    token: 1,
                    poll_after_secs: 0,
                }))
            },
            |pending| {
                polls.set(polls.get() + 1);
                Ok(CreateOutcome::Pending(pending))
            },
        )
        .expect_err("the initial call should consume the whole operation budget");

        assert_eq!(error.kind, ProviderErrorKind::Timeout);
        assert_eq!(polls.get(), 0, "an over-budget operation must not poll");
    }

    #[test]
    fn parse_resource_id_standard() {
        let id = parse_resource_id_string("awscc.iam.role.my-role");
        assert_eq!(id.provider, "awscc");
        assert_eq!(id.resource_type, "iam.role");
        assert_eq!(id.identity, "my-role");
    }

    #[test]
    fn parse_resource_id_three_part_type() {
        let id = parse_resource_id_string("awscc.ec2.ipam.test-ipam");
        assert_eq!(id.provider, "awscc");
        assert_eq!(id.resource_type, "ec2.ipam");
        assert_eq!(id.identity, "test-ipam");
    }

    #[test]
    fn parse_resource_id_with_prefix_name() {
        let id = parse_resource_id_string("awscc.s3.Bucket.carina-acc-test-abc123");
        assert_eq!(id.provider, "awscc");
        assert_eq!(id.resource_type, "s3.Bucket");
        assert_eq!(id.identity, "carina-acc-test-abc123");
    }

    #[test]
    fn parse_resource_id_aws_provider() {
        let id = parse_resource_id_string("aws.s3.Bucket.my-bucket");
        assert_eq!(id.provider, "aws");
        assert_eq!(id.resource_type, "s3.Bucket");
        assert_eq!(id.identity, "my-bucket");
    }

    #[test]
    fn parse_resource_id_no_dots() {
        let id = parse_resource_id_string("simple");
        assert_eq!(id.provider, "");
        assert_eq!(id.resource_type, "");
        assert_eq!(id.identity, "simple");
    }

    #[test]
    fn parse_resource_id_two_parts() {
        let id = parse_resource_id_string("provider.name");
        assert_eq!(id.provider, "provider");
        assert_eq!(id.resource_type, "");
        assert_eq!(id.identity, "name");
    }
}
