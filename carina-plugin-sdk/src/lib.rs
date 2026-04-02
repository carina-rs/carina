//! Carina provider SDK for building external process providers.
//!
//! Implement the `CarinaProvider` trait and call `carina_plugin_sdk::run(provider)`
//! in your `main()` to start the JSON-RPC server loop.

pub use carina_provider_protocol as protocol;
pub use carina_provider_protocol::types;

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod wasm_guest;

#[cfg(target_arch = "wasm32")]
pub mod wasi_http;

use carina_provider_protocol::jsonrpc::{Notification, Request, Response};
use carina_provider_protocol::methods;
use carina_provider_protocol::types::*;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

/// Trait that provider authors implement.
pub trait CarinaProvider {
    /// Return provider name and display name.
    fn info(&self) -> ProviderInfo;

    /// Return the list of optional capabilities this provider supports.
    /// Possible values: "normalize_desired", "normalize_state",
    /// "hydrate_read_state", "merge_default_tags".
    fn capabilities(&self) -> Vec<String> {
        vec![]
    }

    /// Return all resource schemas this provider supports.
    fn schemas(&self) -> Vec<ResourceSchema>;

    /// Validate provider configuration attributes.
    fn validate_config(&self, attrs: &HashMap<String, Value>) -> Result<(), String>;

    /// Initialize the provider with configuration.
    /// Called once before any CRUD operations.
    fn initialize(&mut self, attrs: &HashMap<String, Value>) -> Result<(), String> {
        let _ = attrs;
        Ok(())
    }

    /// Read current state of a resource.
    fn read(&self, id: &ResourceId, identifier: Option<&str>) -> Result<State, ProviderError>;

    /// Create a new resource.
    fn create(&self, resource: &Resource) -> Result<State, ProviderError>;

    /// Update an existing resource.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> Result<State, ProviderError>;

    /// Delete an existing resource.
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> Result<(), ProviderError>;

    /// Normalize desired resources (optional).
    fn normalize_desired(&self, resources: Vec<Resource>) -> Vec<Resource> {
        resources
    }

    /// Normalize read-back state (optional).
    fn normalize_state(&self, states: HashMap<String, State>) -> HashMap<String, State> {
        states
    }

    /// Hydrate read state with saved attributes that APIs don't return.
    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, State>,
        saved_attrs: &HashMap<String, HashMap<String, Value>>,
    ) {
        let _ = (states, saved_attrs);
    }

    /// Merge provider default_tags into resources.
    fn merge_default_tags(
        &self,
        resources: &mut Vec<Resource>,
        default_tags: &HashMap<String, Value>,
        schemas: &Vec<ResourceSchema>,
    ) {
        let _ = (resources, default_tags, schemas);
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

        let response = dispatch(&mut provider, &request);

        let resp_json = serde_json::to_string(&response).expect("Failed to serialize response");
        writeln!(writer, "{resp_json}").expect("Failed to write response");
        writer.flush().expect("Failed to flush");

        // Exit after shutdown
        if request.method == "shutdown" {
            break;
        }
    }
}

fn dispatch(provider: &mut impl CarinaProvider, request: &Request) -> Response {
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
            match provider.read(&params.id, params.identifier.as_deref()) {
                Ok(state) => Response::success(id, methods::ReadResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "create" => {
            let params: methods::CreateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.create(&params.resource) {
                Ok(state) => Response::success(id, methods::CreateResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "update" => {
            let params: methods::UpdateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.update(&params.id, &params.identifier, &params.from, &params.to) {
                Ok(state) => Response::success(id, methods::UpdateResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "delete" => {
            let params: methods::DeleteParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.delete(&params.id, &params.identifier, &params.lifecycle) {
                Ok(()) => Response::success(id, methods::DeleteResult { ok: true }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "normalize_desired" => {
            let params: methods::NormalizeDesiredParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let resources = provider.normalize_desired(params.resources);
            Response::success(id, methods::NormalizeDesiredResult { resources })
        }

        "normalize_state" => {
            let params: methods::NormalizeStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let states = provider.normalize_state(params.states);
            Response::success(id, methods::NormalizeStateResult { states })
        }

        "hydrate_read_state" => {
            let params: methods::HydrateReadStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let mut states = params.states;
            provider.hydrate_read_state(&mut states, &params.saved_attrs);
            Response::success(id, methods::HydrateReadStateResult { states })
        }

        "merge_default_tags" => {
            let params: methods::MergeDefaultTagsParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let mut resources = params.resources;
            provider.merge_default_tags(&mut resources, &params.default_tags, &params.schemas);
            Response::success(id, methods::MergeDefaultTagsResult { resources })
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
