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
    AppliedIdentifier, AttributeType, CompletionValue, CreateRequest, DeleteRequest,
    OperationConfig, PersistedState, PersistedValue, ProviderError, ProviderErrorKind,
    ProviderInfo, ReadOutcome, ReadRequest, Resource, ResourceId, ResourceSchema, State,
    TypeIdentity, UpdateRequest, Value,
};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

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
///
/// A create, update, delete, or poll call is one short step that must finish
/// well under the host's per-call hard timeout; providers must never run an
/// in-call waiter. A poll may return another `Pending`. The host supplies the
/// original request on every poll, so `token` must contain only opaque
/// continuation metadata and must never contain request or secret data.
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
    /// `identifier` is the validated, non-empty provider identifier of an
    /// already managed resource. The result distinguishes a missing resource
    /// from a present observation that also carries a non-empty identifier.
    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError>;

    /// Read a data source resource.
    ///
    /// Unlike [`CarinaProvider::read`], this receives the full [`Resource`]
    /// so the plugin can see user-supplied input attributes (e.g. the
    /// `user_name` for `aws.identitystore.user`).
    ///
    /// Each provider must implement this explicitly. Data sources do not have
    /// a managed-resource identifier, so implementations must not manufacture
    /// one merely to delegate to [`CarinaProvider::read`].
    fn read_data_source(&self, resource: &Resource) -> Result<State, ProviderError>;

    /// Perform one short create step.
    ///
    /// Return [`CreateOutcome::Pending`] when the external operation has not
    /// settled. Do not wait or poll inside this call; it must finish well under
    /// the host's per-call hard timeout.
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError>;

    /// Perform one short poll step for a pending create operation.
    ///
    /// The host passes the original request on every poll. This method may
    /// return [`CreateOutcome::Pending`] again and must not wait in-call.
    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        pending: Pending<Self::CreateToken>,
    ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError>;

    /// Perform one short update step.
    ///
    /// Providers MUST NOT modify any attribute that is not represented in
    /// `request.patch.ops`. The patch is the sole source of truth for the
    /// update payload.
    ///
    /// Return [`UpdateOutcome::Pending`] when the external operation has not
    /// settled. Do not wait or poll inside this call; it must finish well under
    /// the host's per-call hard timeout.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError>;

    /// Perform one short poll step for a pending update operation.
    ///
    /// The host passes the original request and identifier on every poll. This
    /// method may return [`UpdateOutcome::Pending`] again and must not wait
    /// in-call.
    fn poll_update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        pending: Pending<Self::UpdateToken>,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError>;

    /// Perform one short delete step.
    ///
    /// Return [`DeleteOutcome::Pending`] when the external operation has not
    /// settled. Do not wait or poll inside this call; it must finish well under
    /// the host's per-call hard timeout.
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: DeleteRequest,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError>;

    /// Perform one short poll step for a pending delete operation.
    ///
    /// The host passes the original request and identifier on every poll. This
    /// method may return [`DeleteOutcome::Pending`] again and must not wait
    /// in-call.
    fn poll_delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
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

    fn into_step(self)
    -> carina_provider_protocol::OperationStep<Self::Output, Self::PendingState>;
}

impl<T> DrivenOutcome for CreateOutcome<T> {
    type Output = carina_provider_protocol::CreateOutcome;
    type PendingState = Pending<T>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Self::PendingState> {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(outcome),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.poll_after_secs,
                pending,
            },
        }
    }
}

impl<T> DrivenOutcome for UpdateOutcome<T> {
    type Output = carina_provider_protocol::UpdateOutcome;
    type PendingState = Pending<T>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Self::PendingState> {
        match self {
            Self::Done(outcome) => carina_provider_protocol::OperationStep::Done(outcome),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.poll_after_secs,
                pending,
            },
        }
    }
}

impl<T> DrivenOutcome for DeleteOutcome<T> {
    type Output = ();
    type PendingState = Pending<T>;

    fn into_step(
        self,
    ) -> carina_provider_protocol::OperationStep<Self::Output, Self::PendingState> {
        match self {
            Self::Deleted => carina_provider_protocol::OperationStep::Done(()),
            Self::Pending(pending) => carina_provider_protocol::OperationStep::Pending {
                poll_after_secs: pending.poll_after_secs,
                pending,
            },
        }
    }
}

fn block_on_ready<F: std::future::Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    match std::future::Future::poll(future.as_mut(), &mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => {
            unreachable!("the blocking JSON-RPC driver only supplies ready futures")
        }
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
    let outcome = block_on_ready(carina_provider_protocol::drive_pending(
        budget,
        async move { initial().map(DrivenOutcome::into_step) },
        |pending| std::future::ready(poll(pending).map(DrivenOutcome::into_step)),
        |delay| {
            std::thread::sleep(delay);
            std::future::ready(())
        },
    ))?;
    match outcome {
        carina_provider_protocol::DriveOutcome::Done(output) => Ok(output),
        carina_provider_protocol::DriveOutcome::OverBudget => Err(operation_timeout(id, operation)),
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
            match provider.read(&params.id, &params.identifier, params.request) {
                Ok(outcome) => Response::success(id, methods::ReadResult { outcome }),
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
    use carina_provider_protocol::types::{AppliedIdentifier, AppliedState};

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

    struct PendingDispatchProvider {
        create_polls: std::cell::Cell<usize>,
        create_pending_polls: u8,
        schemas: Vec<ResourceSchema>,
    }

    impl Default for PendingDispatchProvider {
        fn default() -> Self {
            Self {
                create_polls: std::cell::Cell::new(0),
                create_pending_polls: 1,
                schemas: Vec::new(),
            }
        }
    }

    impl PendingDispatchProvider {
        fn with_pending_polls_and_schemas(
            create_pending_polls: u8,
            schemas: Vec<ResourceSchema>,
        ) -> Self {
            Self {
                create_polls: std::cell::Cell::new(0),
                create_pending_polls,
                schemas,
            }
        }

        fn completed_create(id: &ResourceId) -> CreateOutcome<u8> {
            CreateOutcome::Done(protocol::CreateOutcome::Success {
                state: AppliedState {
                    id: id.clone(),
                    identifier: AppliedIdentifier::new("created")
                        .expect("the test identifier is non-empty"),
                    attributes: HashMap::new(),
                },
            })
        }
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
            self.schemas.clone()
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
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> Result<ReadOutcome, ProviderError> {
            unreachable!("read is not used by this test")
        }

        fn read_data_source(&self, _resource: &Resource) -> Result<State, ProviderError> {
            unreachable!("read_data_source is not used by this test")
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
            if self.create_pending_polls == 0 {
                return Ok(Self::completed_create(id));
            }
            Ok(CreateOutcome::Pending(Pending {
                token: self.create_pending_polls,
                poll_after_secs: 1,
            }))
        }

        fn poll_create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            pending: Pending<Self::CreateToken>,
        ) -> Result<CreateOutcome<Self::CreateToken>, ProviderError> {
            self.create_polls.set(self.create_polls.get() + 1);
            if pending.token > 1 {
                return Ok(CreateOutcome::Pending(Pending {
                    token: pending.token - 1,
                    poll_after_secs: 1,
                }));
            }
            Ok(Self::completed_create(id))
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            unreachable!("update is not used by this test")
        }

        fn poll_update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            pending: Pending<Self::UpdateToken>,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            match pending.token {}
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
        ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
            unreachable!("delete is not used by this test")
        }

        fn poll_delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
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

    fn operation_schema(resource_type: &str, create_timeout_secs: u64) -> ResourceSchema {
        ResourceSchema {
            resource_type: resource_type.to_string(),
            attributes: HashMap::new(),
            description: None,
            kind: types::SchemaKind::Managed,
            unique_name: types::UniqueNameSpec::default(),
            operation_config: Some(OperationConfig {
                create_timeout_secs: Some(create_timeout_secs),
                ..OperationConfig::default()
            }),
            validators: Vec::new(),
            exclusive_required: Vec::new(),
            defs: std::collections::BTreeMap::new(),
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
    fn json_rpc_dispatch_allows_create_to_settle_on_final_poll_at_deadline() {
        let resource_type = "test.short_budget";
        let mut provider = PendingDispatchProvider::with_pending_polls_and_schemas(
            2,
            vec![operation_schema(resource_type, 2)],
        );
        let operation_configs = operation_config_map(provider.schemas());
        let id = ResourceId {
            provider: "mock".to_string(),
            resource_type: resource_type.to_string(),
            identity: "final-deadline-poll".to_string(),
        };
        let request = Request::new(
            8,
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
        assert_eq!(provider.create_polls.get(), 2);
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn schemas_configure_operation_budget_lookup_for_resource_type() {
        let resource_type = "test.short_budget";
        let provider = PendingDispatchProvider::with_pending_polls_and_schemas(
            2,
            vec![operation_schema(resource_type, 1)],
        );
        let operation_configs = operation_config_map(provider.schemas());
        let id = ResourceId {
            provider: "mock".to_string(),
            resource_type: resource_type.to_string(),
            identity: "over-budget".to_string(),
        };
        let request = CreateRequest {
            resource: Resource {
                id: id.clone(),
                attributes: HashMap::new(),
                directives: protocol::Directives::default(),
            },
        };
        let budget = operation_budget(
            &operation_configs,
            &id,
            carina_provider_protocol::OperationKind::Create,
        );

        assert_eq!(budget, std::time::Duration::from_secs(1));
        let error = drive_operation::<CreateOutcome<u8>, _, _>(
            &id,
            "create",
            budget,
            || provider.create(&id, request.clone()),
            |pending| provider.poll_create(&id, request.clone(), pending),
        )
        .expect_err("the schema-configured budget should expire before the second poll");

        assert_eq!(error.kind, ProviderErrorKind::Timeout);
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
