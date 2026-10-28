---
title: "Implementing Operations"
description: "How a Carina provider implements read, create, update and delete: identifiers, the create typestate, patches, long-running operations, errors and secrets."
---

This page walks through the operations of `widget`, an example provider for an imaginary API with one resource type. The API creates widgets asynchronously: a create call returns an operation ID, and the widget exists once that operation finishes. That is the shape most cloud APIs have, and it exercises every part of the create typestate described in the [Overview](/providers/writing/overview/#short-calls-the-host-waits).

The provider holds an API client behind a trait, so the same code runs against the real client in the component and against a fake in [tests](/providers/writing/testing/). All code on this page compiles and its tests pass against the carina repository at commit `6a7a4c32`. It shows the operations only; the provider still needs `info`, `schemas` and the rest of the trait, and the `export_provider!` line from [Your First Provider](/providers/writing/getting-started/).

## The API client

```rust
//! Stand-in for the client of the API your provider manages.

pub struct Widget {
    pub id: String,
    pub name: String,
    pub size: i64,
}

pub enum ApiError {
    /// The API rejected the request; nothing was changed.
    Rejected(String),
    /// The resource does not exist.
    NotFound,
    /// The outcome is unknown: a timeout, a dropped connection, a 5xx.
    Unknown(String),
}

pub enum CreateStatus {
    InProgress,
    Done { id: String },
    Failed { id: Option<String>, reason: String },
}

pub trait Api {
    /// Starts creating a widget and returns the API's operation ID.
    fn start_create(&self, name: &str, size: i64) -> Result<String, ApiError>;
    fn create_status(&self, operation: &str) -> Result<CreateStatus, ApiError>;
    fn find_by_name(&self, name: &str) -> Result<Option<Widget>, ApiError>;
    fn get(&self, id: &str) -> Result<Widget, ApiError>;
    fn set_size(&self, id: &str, size: i64) -> Result<(), ApiError>;
    fn delete(&self, id: &str) -> Result<(), ApiError>;
}
```

`ApiError` separates the two failures that matter to Carina: `Rejected`, where the API certainly did nothing, and `Unknown`, where it may have.

## Helpers

```rust
// --- errors ---------------------------------------------------------------

fn error(kind: ProviderErrorKind, message: impl Into<String>) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
        resource_id: None,
        cause: None,
        provider_name: Some("widget".to_string()),
        operation: None,
        status: None,
        code: None,
        request_id: None,
    }
}

fn api_error(operation: &str, err: ApiError) -> ProviderError {
    let (kind, message) = match err {
        ApiError::Rejected(message) => (ProviderErrorKind::InvalidInput, message),
        ApiError::NotFound => (ProviderErrorKind::NotFound, "widget not found".to_string()),
        ApiError::Unknown(message) => (ProviderErrorKind::ApiError, message),
    };
    ProviderError {
        operation: Some(operation.to_string()),
        ..error(kind, message)
    }
}

// --- attributes -----------------------------------------------------------

fn attributes(widget: &Widget) -> HashMap<String, Value> {
    HashMap::from([
        ("name".to_string(), Value::String(widget.name.clone())),
        ("size".to_string(), Value::Int(widget.size)),
    ])
}

fn desired(resource: &Resource) -> Result<(String, i64), ProviderError> {
    match (
        resource.attributes.get("name"),
        resource.attributes.get("size"),
    ) {
        (Some(Value::String(name)), Some(Value::Int(size))) => Ok((name.clone(), *size)),
        _ => Err(error(
            ProviderErrorKind::InvalidInput,
            "name (string) and size (int) are required",
        )),
    }
}

fn identifier(id: &str) -> Result<AppliedIdentifier, ProviderError> {
    AppliedIdentifier::new(id)
        .map_err(|_| error(ProviderErrorKind::Internal, "the API returned an empty widget ID"))
}
```

`ProviderError` carries a `kind` and a message, and optional cloud-API details: `operation`, `status`, `code` and `request_id`. Fill the details when they come from an API response; Carina shows them on separate lines. The kinds are `InvalidInput`, `ApiError`, `NotFound`, `Timeout` and `Internal`.

## read

```rust
impl<A: Api> WidgetProvider<A> {
    fn read_found(&self, id: &ResourceId, identifier: &AppliedIdentifier) -> Result<ReadOutcome, ProviderError> {
        match self.api.get(identifier.as_str()) {
            Ok(widget) => Ok(ReadOutcome::Found {
                state: AppliedState {
                    id: id.clone(),
                    identifier: identifier.clone(),
                    attributes: attributes(&widget),
                },
            }),
            Err(ApiError::NotFound) => Ok(ReadOutcome::NotFound { id: id.clone() }),
            Err(err) => Err(api_error("widget.Get", err)),
        }
    }
}
```

```rust
fn read(
    &self,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
    _request: ReadRequest,
) -> Result<ReadOutcome, ProviderError> {
    self.read_found(id, identifier)
}
```

Return `ReadOutcome::NotFound` when the resource is gone. Carina treats it as drift and plans to create the resource again. It treats a `NotFound` error from `read` the same way, but returning the outcome says what you mean.

Return every attribute the API reports. Write-only values never come back from an API, and Carina does not expect them in `read`: it keeps the value it last sent.

## create

```rust
fn create(
    &self,
    _id: &ResourceId,
    request: CreateRequest,
    attempt: CreateAttempt,
) -> Result<CreateStep<CreateToken>, CreateVerdict> {
    // Nothing has been sent yet, so a bad request is untracked.
    let (name, size) = match desired(&request.resource) {
        Ok(values) => values,
        Err(err) => return Err(attempt.untracked(err)),
    };
    match self.api.start_create(&name, size) {
        Ok(operation) => Ok(attempt.pending(
            CreateToken::Operation(operation),
            Sent::AllRequestAttributes,
            5,
        )),
        // The API said no: nothing exists.
        Err(ApiError::Rejected(message)) => Err(attempt.untracked(api_error(
            "widget.Create",
            ApiError::Rejected(message),
        ))),
        // The request may have been accepted. Keep the attempt and find
        // out in poll_create.
        Err(_) => Ok(attempt.pending(CreateToken::Unknown, Sent::AllRequestAttributes, 5)),
    }
}
```

The three outcomes follow the rules from the Overview. Bad input is found before anything is sent, so it is `untracked`, and so is an explicit rejection from the API. When the call fails in a way that does not say whether the API accepted it, the provider cannot know whether a widget exists. Reporting `untracked` there could leave a widget that Carina has forgotten, so the provider returns `pending` with a token that says "unknown" and settles it in `poll_create`.

`Sent::AllRequestAttributes` says that the request carried every attribute in the create request. If your API takes some attributes in a later call, name only the ones the create call carried, with `Sent::only([...])`.

The token is stored in state while the create is unfinished:

```rust
/// What the provider needs to find an unfinished create again. The host
/// stores it in state, so it holds no request values or secrets.
#[derive(Debug, Serialize, Deserialize)]
pub enum CreateToken {
    /// The API accepted the request and returned an operation to follow.
    Operation(String),
    /// The create call failed in a way that does not say whether the API
    /// accepted it; look the widget up by its unique name instead.
    Unknown,
}
```

It holds an operation ID or nothing, never request values. On every poll the host passes the original request again, so the token does not need to repeat it.

## poll_create

```rust
fn poll_create(
    &self,
    id: &ResourceId,
    request: CreateRequest,
    poll: Poll<CreateToken>,
) -> Result<CreateStep<CreateToken>, PollVerdict> {
    let (attempt, token) = match poll {
        Poll::Unissued(attempt, token) => (attempt, token),
        // This provider only calls `issued` when it finishes, so an
        // issued poll never happens. A provider that issues early would
        // read the resource here.
        Poll::Issued(issued, token) => return Ok(issued.pending(token, 5)),
    };
    match token {
        CreateToken::Operation(operation) => match self.api.create_status(&operation) {
            Ok(CreateStatus::InProgress) => Ok(attempt.pending(CreateToken::Operation(operation), 5)),
            Ok(CreateStatus::Done { id: widget_id }) => {
                let issued = match identifier(&widget_id) {
                    Ok(identifier) => attempt.issued(identifier),
                    Err(err) => return Err(attempt.undetermined(err)),
                };
                match self.read_found(id, issued.identifier()) {
                    Ok(ReadOutcome::Found { state }) => Ok(issued.succeeded(state.attributes)),
                    Ok(outcome) => Err(issued
                        .failed(
                            Observation::Read(outcome),
                            error(ProviderErrorKind::ApiError, "widget vanished after create"),
                        )
                        .into()),
                    Err(err) => Err(issued.failed(Observation::ReadFailed(err.clone()), err).into()),
                }
            }
            // Created, then failed: the widget exists and Carina must
            // keep it so it can be deleted or retried.
            Ok(CreateStatus::Failed { id: Some(widget_id), reason }) => match identifier(&widget_id) {
                Ok(identifier) => {
                    let issued = attempt.issued(identifier);
                    let observation = match self.read_found(id, issued.identifier()) {
                        Ok(outcome) => Observation::Read(outcome),
                        Err(err) => Observation::ReadFailed(err),
                    };
                    Err(issued.failed(observation, error(ProviderErrorKind::ApiError, reason)).into())
                }
                Err(err) => Err(attempt.undetermined(err)),
            },
            Ok(CreateStatus::Failed { id: None, reason }) => {
                Err(attempt.untracked(error(ProviderErrorKind::ApiError, reason)))
            }
            Err(err) => Err(attempt.undetermined(api_error("widget.CreateStatus", err))),
        },
        CreateToken::Unknown => {
            let name = match desired(&request.resource) {
                Ok((name, _)) => name,
                Err(err) => return Err(attempt.undetermined(err)),
            };
            match self.api.find_by_name(&name) {
                Ok(Some(widget)) => match identifier(&widget.id) {
                    Ok(identifier) => Ok(attempt.issued(identifier).succeeded(attributes(&widget))),
                    Err(err) => Err(attempt.undetermined(err)),
                },
                // Names are unique, so the earlier call created nothing.
                Ok(None) => Err(attempt.untracked(error(
                    ProviderErrorKind::ApiError,
                    "the create request did not reach the API",
                ))),
                Err(err) => Err(attempt.undetermined(api_error("widget.FindByName", err))),
            }
        }
    }
}
```

`attempt.issued(identifier)` is called only once the widget's ID is known, and from then on the code can only finish with `succeeded` or `failed`. A create that failed after the widget existed is `failed` with the identifier, so Carina records the widget and the next plan can retry or delete it. When the provider cannot tell what happened, such as when the status call itself fails, it returns `undetermined`. Carina then keeps the attempt rather than guessing.

For the unknown case, the provider looks the widget up by its name. That works only because widget names are unique in this API; use whatever your API offers to find a request after the fact, such as an idempotency key.

## resume_create

```rust
fn resume_create(
    &self,
    id: &ResourceId,
    _request: CreateRequest,
    resume: Resume<CreateToken>,
) -> Result<ResumeStep<CreateToken>, ResumeVerdict> {
    match resume {
        // Carina was interrupted after the attempt started. Keep
        // polling where it left off.
        Resume::Unissued(attempt, token) => Ok(attempt.pending(token, 0)),
        Resume::Issued(resumed, _token) => match self.read_found(id, resumed.identifier()) {
            Ok(ReadOutcome::Found { state }) => Ok(resumed.completed(
                carina_plugin_sdk::FoundRead::from_applied_state(state),
            )),
            Ok(outcome) => Err(resumed.failed(
                Observation::Read(outcome),
                error(ProviderErrorKind::ApiError, "widget vanished during create"),
            )),
            Err(err) => Err(resumed.failed(Observation::ReadFailed(err.clone()), err)),
        },
    }
}
```

`resume_create` runs when an earlier apply stopped while a create was pending. An attempt without an identifier goes back to polling with its saved token. An attempt that had an identifier is settled by reading the resource. `ResumeAttempt` also has `resend`, which returns a fresh `CreateAttempt` for a provider that knows the earlier request never reached the API.

## update

```rust
fn update(
    &self,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
    request: UpdateRequest,
) -> Result<UpdateOutcome<Never>, ProviderError> {
    for op in &request.patch.ops {
        match op {
            PatchOp::Replace { key, value } | PatchOp::Add { key, value } | PatchOp::Reassert { key, value }
                if key == "size" =>
            {
                let Value::Int(size) = value else {
                    return Err(error(ProviderErrorKind::InvalidInput, "size must be an int"));
                };
                self.api
                    .set_size(identifier.as_str(), *size)
                    .map_err(|err| api_error("widget.SetSize", err))?;
            }
            // `name` is create-only, so Carina replaces the widget
            // instead of sending it here.
            other => {
                return Err(error(
                    ProviderErrorKind::InvalidInput,
                    format!("cannot update {}", other.key()),
                ));
            }
        }
    }
    let state = request.state_after_patch();
    Ok(UpdateOutcome::Done(carina_plugin_sdk::protocol::UpdateOutcome::Success {
        state: AppliedPersistedState {
            id: id.clone(),
            identifier: identifier.clone(),
            attributes: state.attributes,
        },
    }))
}
```

Apply exactly the operations in the patch and nothing else. `request.from` is the last persisted state, and `request.state_after_patch()` returns it with the patch applied, which is usually what the update should report. `Reassert` carries an unchanged write-only value that the API needs again; send it like `Replace`.

`from` holds a secret value as a hash, never the secret itself. If the update needs a secret, it is in the patch.

If the update succeeded but reading the result back failed, return `PartialSuccess` with a `PartialReadDiagnostic` that names the missing attributes, instead of an error. The update did happen, and Carina records it.

## delete

```rust
fn delete(
    &self,
    _id: &ResourceId,
    identifier: &AppliedIdentifier,
    _request: DeleteRequest,
) -> Result<DeleteOutcome<Never>, ProviderError> {
    match self.api.delete(identifier.as_str()) {
        // Already gone is the outcome delete asked for.
        Ok(()) | Err(ApiError::NotFound) => Ok(DeleteOutcome::Deleted),
        Err(err) => Err(api_error("widget.Delete", err)),
    }
}
```

Treat "already gone" as success. Carina does not special-case a `NotFound` error from `delete`, so returning one fails the apply for a resource that no longer exists.

## Long-running update and delete

`update` and `delete` can also return `Pending` with a token and `poll_after_secs`. The host then calls `poll_update` or `poll_delete` with the original request until the operation finishes or the resource's time budget runs out. The token rules are the same as for create. A provider that finishes every update and delete in one call sets the token types to `Never`, as this one does.

## Secrets

Attribute values reach the provider in plain text, including values the configuration wrapped in `secret(...)`. Carina masks them in plan output and stores only a hash in state, but inside the provider they are ordinary strings. Never put attribute values into error messages, `cause`, logs or pending tokens. Error messages and causes are printed, and tokens are written to state.

## Data sources

A schema with `kind: SchemaKind::DataSource` declares a data source. The host calls `read_data_source` with the full resource, because a data source has no identifier and is looked up from its input attributes. Return a `State` with `exists` and the attributes you found. A provider without data sources returns an error, as `widget` does.

## Other trait methods

`required_permissions` feeds Carina's IAM preflight check, which reports missing AWS IAM actions before an apply. Return an empty list unless your provider manages AWS resources. `normalize_desired`, `normalize_state` and `hydrate_read_state` are optional hooks for providers whose API changes values on the way in or out. Their default implementations return the values unchanged, so implement them only when you need them.

## Next steps

[Testing](/providers/writing/testing/) tests these operations against a fake API, then end to end against a real account.
