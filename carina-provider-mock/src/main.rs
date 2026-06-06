use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{BoxFuture, CarinaProvider};
use std::collections::HashMap;
use std::sync::Mutex;

struct MockProcessProvider {
    states: Mutex<HashMap<String, HashMap<String, Value>>>,
}

impl Default for MockProcessProvider {
    fn default() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl MockProcessProvider {
    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.name)
    }

    #[cfg(target_arch = "wasm32")]
    fn run_initialize_http_requests(attrs: &HashMap<String, Value>) -> Result<(), String> {
        let Some(Value::String(url)) = attrs.get("__mock_initialize_http_url") else {
            return Ok(());
        };
        let request_count = match attrs.get("__mock_initialize_http_requests") {
            Some(Value::Int(n)) if *n > 0 => *n as usize,
            _ => 1,
        };
        let concurrency = match attrs.get("__mock_initialize_http_concurrency") {
            Some(Value::Int(n)) if *n > 0 => *n as usize,
            _ => 1,
        };
        let shape = match attrs.get("__mock_initialize_http_shape") {
            Some(Value::String(shape)) => shape.as_str(),
            _ => {
                if concurrency > 1 {
                    "poll-batch"
                } else {
                    "sequential"
                }
            }
        };

        match shape {
            "sequential" => Self::run_sequential_initialize_http_requests(url, request_count),
            "poll-batch" => {
                Self::run_concurrent_initialize_http_requests(url, request_count, concurrency)
            }
            "tokio-join" => {
                Self::run_tokio_join_like_initialize_http_requests(url, request_count, concurrency)
            }
            "spawn-await" => {
                Self::run_spawn_await_like_initialize_http_requests(url, request_count, concurrency)
            }
            "sleep-interleave" => {
                Self::run_sleep_interleave_initialize_http_requests(url, request_count, concurrency)
            }
            other => Err(format!("unknown __mock_initialize_http_shape: {other}")),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn run_sequential_initialize_http_requests(
        url: &str,
        request_count: usize,
    ) -> Result<(), String> {
        for i in 0..request_count {
            let request = http::Request::builder()
                .method("GET")
                .uri(format!("{url}?request={i}"))
                .body(Vec::new())
                .map_err(|e| format!("failed to build initialize HTTP request: {e}"))?;
            let response = carina_plugin_sdk::wasi_http::send_request(request)?;
            if !response.status().is_success() {
                return Err(format!(
                    "initialize HTTP request returned status {}",
                    response.status()
                ));
            }
        }

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn run_concurrent_initialize_http_requests(
        url: &str,
        request_count: usize,
        concurrency: usize,
    ) -> Result<(), String> {
        use wasi::http::types::{FutureIncomingResponse, IncomingBody};
        use wasi::io::poll::{self, Pollable};

        struct PendingResponse {
            request_index: usize,
            future: FutureIncomingResponse,
            pollable: Pollable,
        }

        let mut next_request = 0;
        while next_request < request_count {
            let batch_len = concurrency.min(request_count - next_request);
            let mut pending = Vec::with_capacity(batch_len);
            for request_index in next_request..next_request + batch_len {
                let future = Self::start_wasi_get(url, request_index)?;
                let pollable = future.subscribe();
                pending.push(PendingResponse {
                    request_index,
                    future,
                    pollable,
                });
            }

            while !pending.is_empty() {
                let pollables: Vec<&Pollable> = pending.iter().map(|p| &p.pollable).collect();
                let mut ready = poll::poll(&pollables);
                ready.sort_unstable_by(|a, b| b.cmp(a));
                for index in ready {
                    let PendingResponse {
                        request_index,
                        future,
                        pollable,
                    } = pending.remove(index as usize);
                    drop(pollable);

                    let response = future
                        .get()
                        .ok_or_else(|| {
                            format!(
                                "initialize HTTP request {} not ready after poll-list",
                                request_index
                            )
                        })?
                        .map_err(|()| {
                            format!(
                                "initialize HTTP request {} response already taken",
                                request_index
                            )
                        })?
                        .map_err(|e| {
                            format!("initialize HTTP request {} failed: {e:?}", request_index)
                        })?;
                    drop(future);

                    if response.status() < 200 || response.status() >= 300 {
                        return Err(format!(
                            "initialize HTTP request {} returned status {}",
                            request_index,
                            response.status()
                        ));
                    }
                    if let Ok(body) = response.consume() {
                        let _trailers = IncomingBody::finish(body);
                    }
                }
            }

            next_request += batch_len;
        }

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn run_tokio_join_like_initialize_http_requests(
        url: &str,
        request_count: usize,
        concurrency: usize,
    ) -> Result<(), String> {
        Self::run_interleaved_initialize_http_requests(url, request_count, concurrency, || {
            let mut checksum = 0usize;
            for value in 0..64 {
                checksum = checksum.wrapping_add(value);
            }
            std::hint::black_box(checksum);
        })
    }

    #[cfg(target_arch = "wasm32")]
    fn run_spawn_await_like_initialize_http_requests(
        url: &str,
        request_count: usize,
        concurrency: usize,
    ) -> Result<(), String> {
        let mut next_request = 0;
        while next_request < request_count {
            if next_request + 1 < request_count && concurrency > 1 {
                let outer = Self::start_wasi_get(url, next_request)?;
                let outer_pollable = outer.subscribe();
                Self::run_scoped_initialize_http_request(url, next_request + 1)?;
                Self::finish_ready_response(next_request, outer, outer_pollable)?;
                next_request += 2;
            } else {
                Self::run_scoped_initialize_http_request(url, next_request)?;
                next_request += 1;
            }
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn run_sleep_interleave_initialize_http_requests(
        url: &str,
        request_count: usize,
        concurrency: usize,
    ) -> Result<(), String> {
        Self::run_interleaved_initialize_http_requests(url, request_count, concurrency, || {
            wasi::clocks::monotonic_clock::subscribe_duration(1_000_000).block();
        })
    }

    #[cfg(target_arch = "wasm32")]
    fn run_interleaved_initialize_http_requests(
        url: &str,
        request_count: usize,
        concurrency: usize,
        mut between_polls: impl FnMut(),
    ) -> Result<(), String> {
        use wasi::http::types::{FutureIncomingResponse, IncomingBody};
        use wasi::io::poll::{self, Pollable};

        struct PendingResponse {
            request_index: usize,
            future: FutureIncomingResponse,
            pollable: Pollable,
        }

        let mut next_request = 0;
        while next_request < request_count {
            let batch_len = concurrency.max(1).min(request_count - next_request);
            let mut pending = Vec::with_capacity(batch_len);
            for request_index in next_request..next_request + batch_len {
                let future = Self::start_wasi_get(url, request_index)?;
                let pollable = future.subscribe();
                pending.push(PendingResponse {
                    request_index,
                    future,
                    pollable,
                });
                between_polls();
            }

            while !pending.is_empty() {
                between_polls();
                let pollables: Vec<&Pollable> = pending.iter().map(|p| &p.pollable).collect();
                let mut ready = poll::poll(&pollables);
                between_polls();
                ready.sort_unstable_by(|a, b| b.cmp(a));
                for index in ready {
                    let PendingResponse {
                        request_index,
                        future,
                        pollable,
                    } = pending.remove(index as usize);
                    drop(pollable);

                    let response = future
                        .get()
                        .ok_or_else(|| {
                            format!(
                                "initialize HTTP request {} not ready after poll-list",
                                request_index
                            )
                        })?
                        .map_err(|()| {
                            format!(
                                "initialize HTTP request {} response already taken",
                                request_index
                            )
                        })?
                        .map_err(|e| {
                            format!("initialize HTTP request {} failed: {e:?}", request_index)
                        })?;
                    drop(future);

                    if response.status() < 200 || response.status() >= 300 {
                        return Err(format!(
                            "initialize HTTP request {} returned status {}",
                            request_index,
                            response.status()
                        ));
                    }
                    if let Ok(body) = response.consume() {
                        let _trailers = IncomingBody::finish(body);
                    }
                }
            }

            next_request += batch_len;
        }

        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn run_scoped_initialize_http_request(url: &str, request_index: usize) -> Result<(), String> {
        let future = Self::start_wasi_get(url, request_index)?;
        let pollable = future.subscribe();
        Self::finish_ready_response(request_index, future, pollable)
    }

    #[cfg(target_arch = "wasm32")]
    fn finish_ready_response(
        request_index: usize,
        future: wasi::http::types::FutureIncomingResponse,
        pollable: wasi::io::poll::Pollable,
    ) -> Result<(), String> {
        use wasi::http::types::IncomingBody;

        pollable.block();
        drop(pollable);
        let response = future
            .get()
            .ok_or_else(|| {
                format!(
                    "initialize HTTP request {} not ready after pollable block",
                    request_index
                )
            })?
            .map_err(|()| {
                format!(
                    "initialize HTTP request {} response already taken",
                    request_index
                )
            })?
            .map_err(|e| format!("initialize HTTP request {} failed: {e:?}", request_index))?;
        drop(future);

        if response.status() < 200 || response.status() >= 300 {
            return Err(format!(
                "initialize HTTP request {} returned status {}",
                request_index,
                response.status()
            ));
        }
        if let Ok(body) = response.consume() {
            let _trailers = IncomingBody::finish(body);
        }
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn start_wasi_get(
        url: &str,
        request_index: usize,
    ) -> Result<wasi::http::types::FutureIncomingResponse, String> {
        use wasi::http::outgoing_handler;
        use wasi::http::types::{Fields, Method, OutgoingBody, OutgoingRequest, Scheme};

        let uri = format!("{url}?request={request_index}")
            .parse::<http::Uri>()
            .map_err(|e| format!("invalid initialize HTTP URI: {e}"))?;
        let fields = Fields::from_list(&[("content-length".to_string(), b"0".to_vec())])
            .map_err(|e| format!("failed to create initialize HTTP headers: {e:?}"))?;
        let outgoing_req = OutgoingRequest::new(fields);
        outgoing_req
            .set_method(&Method::Get)
            .map_err(|()| "failed to set initialize HTTP method".to_string())?;
        let scheme = match uri.scheme_str() {
            Some("http") => Scheme::Http,
            Some("https") => Scheme::Https,
            Some(other) => Scheme::Other(other.to_string()),
            None => Scheme::Http,
        };
        outgoing_req
            .set_scheme(Some(&scheme))
            .map_err(|()| "failed to set initialize HTTP scheme".to_string())?;
        let authority = uri.authority().map(|a| a.to_string());
        outgoing_req
            .set_authority(authority.as_deref())
            .map_err(|()| "failed to set initialize HTTP authority".to_string())?;
        let path_and_query = uri
            .path_and_query()
            .map(|pq| pq.to_string())
            .unwrap_or_else(|| "/".to_string());
        outgoing_req
            .set_path_with_query(Some(&path_and_query))
            .map_err(|()| "failed to set initialize HTTP path".to_string())?;

        let outgoing_body = outgoing_req
            .body()
            .map_err(|()| "failed to get initialize HTTP body".to_string())?;
        let future = outgoing_handler::handle(outgoing_req, None)
            .map_err(|e| format!("outgoing-handler error: {e:?}"))?;
        OutgoingBody::finish(outgoing_body, None)
            .map_err(|e| format!("failed to finish initialize HTTP body: {e:?}"))?;
        Ok(future)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_initialize_http_requests(_attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }
}

impl CarinaProvider for MockProcessProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock".into(),
            display_name: "Mock Provider (Process)".into(),
            capabilities: vec![],
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![]
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn initialize<'a>(
        &'a mut self,
        attrs: &'a HashMap<String, Value>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { Self::run_initialize_http_requests(attrs) })
    }

    fn read<'a>(
        &'a self,
        id: &'a ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'a, Result<State, ProviderError>> {
        Box::pin(async move {
            let states = self.states.lock().unwrap();
            let key = Self::resource_key(id);

            if let Some(attrs) = states.get(&key) {
                Ok(State {
                    id: id.clone(),
                    identifier: Some("mock-id".into()),
                    attributes: attrs.clone(),
                    exists: true,
                })
            } else {
                Ok(State {
                    id: id.clone(),
                    identifier: None,
                    attributes: HashMap::new(),
                    exists: false,
                })
            }
        })
    }

    /// Exercise the `read_data_source` path end-to-end through the WASM
    /// bridge: echo the user-supplied inputs back into state plus a
    /// sentinel `__mock_read_data_source__` flag so integration tests can
    /// verify the call was routed through the WASM boundary.
    fn read_data_source<'a>(
        &'a self,
        resource: &'a Resource,
    ) -> BoxFuture<'a, Result<State, ProviderError>> {
        Box::pin(async move {
            let mut attributes = resource.attributes.clone();
            attributes.insert("__mock_read_data_source__".to_string(), Value::Bool(true));
            Ok(State {
                id: resource.id.clone(),
                identifier: Some("mock-id".into()),
                attributes,
                exists: true,
            })
        })
    }

    fn create<'a>(
        &'a self,
        id: &'a ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'a, Result<State, ProviderError>> {
        Box::pin(async move {
            let mut states = self.states.lock().unwrap();
            let key = Self::resource_key(id);
            let resource = request.resource;
            states.insert(key, resource.attributes.clone());

            Ok(State {
                id: id.clone(),
                identifier: Some("mock-id".into()),
                attributes: resource.attributes,
                exists: true,
            })
        })
    }

    fn update<'a>(
        &'a self,
        id: &'a ResourceId,
        _identifier: &'a str,
        request: UpdateRequest,
    ) -> BoxFuture<'a, Result<State, ProviderError>> {
        Box::pin(async move {
            // Apply the patch on top of `from` to construct the post-update
            // attribute map. Also echo the patch op kinds into a sentinel
            // attribute so integration tests can assert the patch
            // round-tripped through the WIT boundary.
            let mut attributes = request.from.attributes.clone();
            let mut applied_op_kinds: Vec<Value> = Vec::with_capacity(request.patch.ops.len());
            for op in &request.patch.ops {
                applied_op_kinds.push(Value::String(format!(
                    "{}:{}",
                    match op.kind {
                        PatchOpKind::Add => "add",
                        PatchOpKind::Replace => "replace",
                        PatchOpKind::Remove => "remove",
                    },
                    op.key,
                )));
                match op.kind {
                    PatchOpKind::Add | PatchOpKind::Replace => {
                        if let Some(value) = op.value.clone() {
                            attributes.insert(op.key.clone(), value);
                        }
                    }
                    PatchOpKind::Remove => {
                        attributes.remove(&op.key);
                    }
                }
            }
            attributes.insert(
                "__mock_patch_ops__".to_string(),
                Value::List(applied_op_kinds),
            );

            let mut states = self.states.lock().unwrap();
            let key = Self::resource_key(id);
            states.insert(key, attributes.clone());

            Ok(State {
                id: id.clone(),
                identifier: Some("mock-id".into()),
                attributes,
                exists: true,
            })
        })
    }

    fn delete<'a>(
        &'a self,
        id: &'a ResourceId,
        _identifier: &'a str,
        _request: DeleteRequest,
    ) -> BoxFuture<'a, Result<(), ProviderError>> {
        Box::pin(async move {
            let mut states = self.states.lock().unwrap();
            let key = Self::resource_key(id);
            states.remove(&key);
            Ok(())
        })
    }

    /// Echo the host-provided `default_tags` back into each resource's
    /// attributes under a sentinel `__mock_merged_default_tags__` key so
    /// integration tests can verify the WIT bridge dispatched the call.
    /// Real providers would call `merge_default_tags_for_provider` here;
    /// the mock provider's job is just to prove the call landed.
    fn merge_default_tags(
        &self,
        resources: &mut Vec<Resource>,
        default_tags: &HashMap<String, Value>,
        _schemas: &Vec<ResourceSchema>,
    ) {
        let snapshot: Vec<Value> = default_tags
            .iter()
            .map(|(k, v)| {
                Value::Map(HashMap::from([
                    ("k".to_string(), Value::String(k.clone())),
                    ("v".to_string(), v.clone()),
                ]))
            })
            .collect();
        for r in resources.iter_mut() {
            r.attributes.insert(
                "__mock_merged_default_tags__".to_string(),
                Value::List(snapshot.clone()),
            );
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    carina_plugin_sdk::run(MockProcessProvider::default());
}

// For WASM: export_provider! macro bridges CarinaProvider to the WIT interface.
// An empty main() is still required for the binary target.
#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(MockProcessProvider, http);

#[cfg(target_arch = "wasm32")]
fn main() {}
