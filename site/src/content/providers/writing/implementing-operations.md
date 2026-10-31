---
title: "Implementing Operations"
description: "How a Carina provider implements read, create, update and delete: identifiers, the create typestate, patches, long-running operations, errors and secrets."
---

This page walks through the operations of `widget`, an example provider for an imaginary API with one resource type. The API creates widgets asynchronously: a create call returns an operation ID, and the widget exists once that operation finishes. That is the shape most cloud APIs have, and it exercises most of the create typestate described in the [Overview](/providers/writing/overview/#short-calls-the-host-waits).

The provider holds an API client behind a trait, so the same code runs against the real client in the component and against a fake in [tests](/providers/writing/testing/). All code on this page compiles and its tests pass against the carina repository at commit `6a7a4c32`; the SDK there is the same as at `6726a1cc`, the commit [Your First Provider](/providers/writing/getting-started/) uses. It shows the operations only; the provider still needs `info`, `schemas` and the rest of the trait.

## The API client

```rust
//! Stand-in for the client of the API your provider manages.

#[derive(Clone)]
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
    /// A fresh idempotency key, such as a UUID.
    fn new_client_token(&self) -> String;
    /// Starts creating a widget and returns the API's operation ID. Calling
    /// it again with the same client token returns the same operation
    /// instead of creating a second widget.
    fn start_create(&self, client_token: &str, name: &str, size: i64) -> Result<String, ApiError>;
    fn create_status(&self, operation: &str) -> Result<CreateStatus, ApiError>;
    fn get(&self, id: &str) -> Result<Widget, ApiError>;
    fn set_size(&self, id: &str, size: i64) -> Result<(), ApiError>;
    fn delete(&self, id: &str) -> Result<(), ApiError>;
}
```

`ApiError` separates the two failures that matter to Carina: `Rejected`, where the API certainly did nothing, and `Unknown`, where it may have. The client also issues idempotency keys: retrying `start_create` with the same client token returns the operation the first call started instead of creating a second widget. Most cloud APIs offer something like this (a client token or request ID), and a provider should use it.

## The provider type

```rust
// The SDK's trait returns `ProviderError` by value; match its allowance.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};

use carina_plugin_sdk::core::schema::ReferenceForms;
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending, Poll,
    PollFinality, PollRequest, PollVerdict, Resume, ResumeStep, ResumeVerdict, Sent, UpdateOutcome,
};
use serde::{Deserialize, Serialize};

pub mod api;
use api::{Api, ApiError, CreateStatus, Widget};

pub struct WidgetProvider<A> {
    pub api: A,
}
```

```rust
impl<A: Api> CarinaProvider for WidgetProvider<A> {
    type CreateToken = CreateToken;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn reference_forms(&self) -> ReferenceForms {
        ReferenceForms::new()
    }

    fn initialize(
        &mut self,
        _attrs: &BTreeMap<String, Value>,
    ) -> Result<ReferenceScope, String> {
        Ok(ReferenceScope::Unavailable)
    }

    // info, schemas, and the operations below
}
```

`CreateToken` is defined below. Updates and deletes finish in one call, so their token types are `Never`.

When an initialized instance knows its reference namespace, return
`ReferenceScope::available(partition, region, account)` from `initialize`.
It returns `ReferenceScopeError` when a value is empty or contains `{` or `}`;
map that error to `String` for the trait method. Return
`ReferenceScope::Unavailable` when the namespace is not known; under
`Unavailable`, the provider's reference forms are inert and do not expand
short names during comparison.

The component exports one concrete type, for example `export_provider!(WidgetProvider<HttpApi>, http)` with your real client as `HttpApi`; the `http` form also needs `wasi = "0.14"` in the `wasm32` dependencies, as the Overview explains. `export_provider!` builds the provider with `Default`, so that type needs a `Default` implementation, written in the same crate as `WidgetProvider` (Rust's orphan rule rejects it in a separate binary crate), and the client should take its endpoint and credentials in `initialize`, from the `provider` block. Remember that the host only lets requests reach AWS hosts today; see the [Overview](/providers/writing/overview/#what-a-provider-is).

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
    AppliedIdentifier::new(id).map_err(|_| {
        error(
            ProviderErrorKind::Internal,
            "the API returned an empty widget ID",
        )
    })
}
```

`ProviderError` carries a `kind` and a message, and optional details: `cause` (the underlying error, flattened to text), and from a cloud API, `operation`, `status`, `code` and `request_id`. Fill the details when they come from an API response; Carina shows them on separate lines. The kinds are `InvalidInput`, `ApiError`, `NotFound`, `Timeout` and `Internal`.

## read

```rust
impl<A: Api> WidgetProvider<A> {
    fn read_found(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
    ) -> Result<ReadOutcome, ProviderError> {
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

`read_found` is a separate method because `poll_create` and `resume_create` read the widget back the same way. Return `ReadOutcome::NotFound` when the resource is gone. Carina treats it as drift and plans to create the resource again. It treats a `NotFound` error from `read` the same way, but returning the outcome says what you mean.

Return every attribute the API reports. Write-only values never come back from an API. Mark them `write_only` in the schema, and Carina does not expect them in `read`: it keeps the value it last sent. An attribute the API never returns that is not marked shows up as a change in every plan.

## create

```rust
fn create(
    &self,
    _id: &ResourceId,
    request: CreateRequest,
    attempt: CreateAttempt,
) -> Result<CreateStep<CreateToken>, CreateVerdict<CreateToken>> {
    // Nothing has been sent yet, so a bad request is untracked.
    let (name, size) = match desired(&request.resource) {
        Ok(values) => values,
        Err(err) => return Err(attempt.untracked(err)),
    };
    let client_token = self.api.new_client_token();
    match self.api.start_create(&client_token, &name, size) {
        Ok(operation) => Ok(attempt.pending(
            CreateToken::Operation(operation),
            Sent::AllRequestAttributes,
            5,
        )),
        // The API said no: nothing exists.
        Err(err @ ApiError::Rejected(_)) => {
            Err(attempt.untracked(api_error("widget.Create", err)))
        }
        // The request may have been accepted. Keep the attempt and
        // retry it with the same client token in poll_create.
        Err(_) => Ok(attempt.pending(
            CreateToken::Starting { client_token },
            Sent::AllRequestAttributes,
            5,
        )),
    }
}
```

The three outcomes follow the rules from the Overview. Bad input is found before anything is sent, so it is `untracked`, and so is an explicit rejection from the API. When the call fails in a way that does not say whether the API accepted it, the provider cannot know whether a widget exists. Reporting `untracked` there could leave a widget that Carina has forgotten, so the provider returns `pending` with the client token and retries the call in `poll_create`.

`Sent::AllRequestAttributes` says that the request carried every attribute in the create request. If your API takes some attributes in a later call, name only the ones the create call carried, with `Sent::only([...])`.

The token is stored in state while the create is unfinished:

```rust
/// What the provider needs to find an unfinished create again. The host
/// stores it in state, so it holds no request values or secrets.
#[derive(Debug, Serialize, Deserialize)]
pub enum CreateToken {
    /// The create call has not been confirmed. Retrying it with the same
    /// client token either starts the create or returns the operation the
    /// first call started.
    Starting { client_token: String },
    /// The API accepted the request and returned an operation to follow.
    Operation(String),
}
```

It holds a client token or an operation ID, never request values. Deriving `Serialize` and `Deserialize` needs `serde` with the `derive` feature: `serde = { version = "1", features = ["derive"] }`. Within one apply, every poll receives the original request again, so the token does not need to repeat it. A create resumed by a later apply receives the request built from the configuration as it is then, which may have changed.

## poll_create

```rust
fn poll_create(
    &self,
    id: &ResourceId,
    request: CreateRequest,
    poll: Poll<CreateToken>,
    poll_request: PollRequest,
) -> Result<CreateStep<CreateToken>, PollVerdict<CreateToken>> {
    let (attempt, token) = match poll {
        Poll::Unissued(attempt, token) => (attempt, token),
        // The widget's ID is known and the create has finished; only
        // reading it back is left.
        Poll::Issued(issued, token) => {
            return match self.read_found(id, issued.identifier()) {
                Ok(ReadOutcome::Found { state }) => Ok(issued.succeeded(state.attributes)),
                Ok(outcome) => Err(issued
                    .failed(
                        Observation::Read(outcome),
                        error(ProviderErrorKind::ApiError, "widget vanished after create"),
                    )
                    .into()),
                // The deadline poll must finish. Preserve the applied state
                // and tell Carina which value still needs a later refresh.
                Err(_) if poll_request.finality == PollFinality::FinalAttempt => {
                    Ok(issued.partial(
                        request.resource.attributes,
                        PartialReadDiagnostic {
                            reason: "widget read-back is still pending".to_string(),
                            missing_attributes: vec!["status".to_string()],
                        },
                    ))
                }
                // A read error before the deadline: try again in read-back phase.
                Err(_) => Ok(issued.pending_read_back(token, 5)),
            };
        }
    };
    match token {
        CreateToken::Starting { client_token } => {
            let (name, size) = match desired(&request.resource) {
                Ok(values) => values,
                Err(err) => {
                    return Err(attempt.preserve(
                        CreateToken::Starting { client_token },
                        5,
                        err,
                    ));
                }
            };
            match self.api.start_create(&client_token, &name, size) {
                Ok(operation) => Ok(attempt.pending(CreateToken::Operation(operation), 5)),
                // A rejected retry does not prove the first call created
                // nothing: the API may refuse a reused client token, for
                // example because the configuration changed in between.
                Err(err @ ApiError::Rejected(_)) => {
                    Err(attempt.preserve(
                        CreateToken::Starting { client_token },
                        5,
                        api_error("widget.Create", err),
                    ))
                }
                Err(_) => Ok(attempt.pending(CreateToken::Starting { client_token }, 5)),
            }
        }
        CreateToken::Operation(operation) => match self.api.create_status(&operation) {
            Ok(CreateStatus::InProgress) => {
                Ok(attempt.pending(CreateToken::Operation(operation), 5))
            }
            Ok(CreateStatus::Done { id: widget_id }) => match identifier(&widget_id) {
                // Record the identifier now and read the widget on the
                // next poll, through the Poll::Issued arm above.
                Ok(identifier) => Ok(attempt
                    .issued(identifier)
                    .pending_read_back(CreateToken::Operation(operation), 0)),
                Err(err) => Err(attempt.preserve(
                    CreateToken::Operation(operation),
                    5,
                    err,
                )),
            },
            // The provider reported an identifier, then the operation failed.
            // The read-back observation says whether the widget exists.
            Ok(CreateStatus::Failed {
                id: Some(widget_id),
                reason,
            }) => match identifier(&widget_id) {
                Ok(identifier) => {
                    let issued = attempt.issued(identifier);
                    let observation = match self.read_found(id, issued.identifier()) {
                        Ok(outcome) => Observation::Read(outcome),
                        Err(err) => Observation::ReadFailed(err),
                    };
                    Err(issued
                        .failed(observation, error(ProviderErrorKind::ApiError, reason))
                        .into())
                }
                Err(err) => Err(attempt.preserve(
                    CreateToken::Operation(operation),
                    5,
                    err,
                )),
            },
            Ok(CreateStatus::Failed { id: None, reason }) => {
                Err(attempt.untracked(error(ProviderErrorKind::ApiError, reason)))
            }
            // The status call failed; the operation is still there.
            Err(_) => Ok(attempt.pending(CreateToken::Operation(operation), 5)),
        },
    }
}
```

A `Starting` token retries the create with the same client token. Because the API treats the retry as the same request, it either starts the create now or returns the operation the first call started, so the provider does not create a second widget or claim one it did not create. That holds only while the API still remembers the client token; many APIs forget idempotency keys after a day or so.

A rejected retry is `preserve`, not `untracked`. The first call may have created the widget, and the API can refuse the reused token for reasons of its own, for example because the configuration changed before a later apply resumed the create. `preserve` carries the same continuation plus the error, so Carina keeps the attempt while surfacing why this poll stopped. Any other failure keeps the attempt pending.

When the operation finishes, the provider calls `attempt.issued(identifier)` and returns `pending_read_back` once more. That records the identifier right away, starts the host's read-back budget, and makes the next poll go to the `Poll::Issued` arm. `poll_request.finality` is `Continue` on ordinary polls and `FinalAttempt` on the one poll at the active deadline. Before that deadline a failed read remains `pending_read_back`; on the final attempt the provider must return success, partial success, or an error. Returning another read-back continuation is a protocol violation, so this example returns `partial` with the known state and missing attributes.

A create for which the provider reported an identifier is `failed` with that identifier, so Carina records the identifier plus only observed or sent values, and the next plan re-checks it. `failed` also takes an `Observation`: what the provider saw when it read the widget, either `Observation::Read` with the read outcome or `Observation::ReadFailed` with the read error. The observation passed to `failed` says whether the resource was found and supplies any observed state Carina can record. `failed` returns a `CreateVerdict<CreateToken>`, and `.into()` turns it into the `PollVerdict<CreateToken>` that `poll_create` returns.

Two outcomes after `issued` are not used here. `partial` finishes a create whose read-back was incomplete, with a `PartialReadDiagnostic` naming the missing attributes. `discarded` reports that the provider deleted the resource it had just created, so Carina keeps no record of it. A create that failed without producing a widget is `untracked`. When the status call itself fails, the operation is still there, so the provider polls again. When a provider call has a usable continuation but also needs to report an error, `preserve` returns the error-bearing in-flight verdict. Provider code does not produce `undetermined`; guest transport glue reserves that wire arm for a request or token it cannot decode, when it has no newer continuation to return.

## resume_create

```rust
fn resume_create(
    &self,
    id: &ResourceId,
    _request: CreateRequest,
    resume: Resume<CreateToken>,
) -> Result<ResumeStep<CreateToken>, ResumeVerdict<CreateToken>> {
    match resume {
        // Carina was interrupted after the attempt started. Keep
        // polling where it left off.
        Resume::Unissued(attempt, token) => Ok(attempt.pending(token, 0)),
        Resume::Issued(resumed, token) => match self.read_found(id, resumed.identifier()) {
            Ok(ReadOutcome::Found { state }) => {
                Ok(resumed.completed(carina_plugin_sdk::FoundRead::from_applied_state(state)))
            }
            Ok(outcome) => Err(resumed.failed(
                Observation::Read(outcome),
                error(ProviderErrorKind::ApiError, "widget vanished during create"),
            )),
            // A read error: try again on the next poll.
            Err(_) => Ok(resumed.pending(token, 5)),
        },
    }
}
```

`resume_create` runs when an earlier apply stopped while a create was pending. An attempt without an identifier goes back to polling with its saved token. An attempt that had an identifier is settled by reading the resource, and stays pending if the read fails. `ResumeAttempt` also has `resend`, which returns a fresh `CreateAttempt` for a provider that knows the earlier request never reached the API.

## update

```rust
fn update(
    &self,
    id: &ResourceId,
    identifier: &AppliedIdentifier,
    request: UpdateRequest,
) -> Result<UpdateOutcome<Never>, ProviderError> {
    // Check the whole patch before calling the API, so an op this
    // provider cannot apply does not leave the others half done.
    let mut new_size = None;
    for op in &request.patch.ops {
        match op {
            PatchOp::Add { key, value }
            | PatchOp::Replace { key, value }
            | PatchOp::Reassert { key, value }
                if key == "size" =>
            {
                let Value::Int(size) = value else {
                    return Err(error(
                        ProviderErrorKind::InvalidInput,
                        "size must be an int",
                    ));
                };
                new_size = Some(*size);
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
    if let Some(size) = new_size {
        self.api
            .set_size(identifier.as_str(), size)
            .map_err(|err| api_error("widget.SetSize", err))?;
    }
    let state = request.state_after_patch();
    Ok(UpdateOutcome::Done(
        carina_plugin_sdk::protocol::UpdateOutcome::Success {
            state: AppliedPersistedState {
                id: id.clone(),
                identifier: identifier.clone(),
                attributes: state.attributes,
            },
        },
    ))
}
```

Apply exactly the operations in the patch and nothing else. Check every operation before the first API call, so an operation the provider cannot apply does not leave the others half done. `request.from` is the last persisted state, and `request.state_after_patch()` returns it with the patch applied, which is usually what the update should report. The SDK's `UpdateOutcome` is either `Done` or `Pending`. `Done` wraps the protocol's own `UpdateOutcome`, whose `Success` variant carries an `AppliedPersistedState`: the resource's identifier and its attributes after the update. `Add` and `Replace` both mean "set this value". `Remove` means the attribute was dropped from the configuration: unset it, or reset it to the API's default, and `state_after_patch` drops the key. `Reassert` carries an unchanged write-only value that the API needs again; send it like `Replace`. In `widget`, `size` is required and `name` is create-only, so no `Remove` and no change to `name` ever reaches `update`; the catch-all arm rejects anything the schema should have prevented.

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

`update` and `delete` can also return `Pending` with a token and `poll_after_secs`. The host then calls `poll_update` or `poll_delete` with the original request until the operation finishes or the resource's time budget runs out. `poll_update` receives the same `PollRequest` finality as `poll_create`, and update read-back uses `Pending::read_back`; `poll_delete` is unchanged. The token rules are the same as for create. A provider that finishes every update and delete in one call sets the token types to `Never`, as this one does.

## Secrets

Attribute values reach the provider in plain text, including values the configuration wrapped in `secret(...)`. Carina masks them in plan output and stores only a hash in state, but inside the provider they are ordinary strings. Never put attribute values into error messages, `cause`, logs or pending tokens. That includes API error text that repeats request values back, which many validation errors do; replace it with your own message when the request contains a secret. Error messages and causes are printed, and create tokens are written to state.

## Data sources

A schema with `kind: SchemaKind::DataSource` declares a data source. The host calls `read_data_source` with the full resource, because a data source has no identifier and is looked up from its input attributes. Return a `State` with `exists` and the attributes you found. A provider without data sources returns an error, as `widget` does.

## Other trait methods

`reference_forms` is required; return `ReferenceForms::new()` when the provider declares no short-to-long reference forms. `required_permissions` feeds the IAM preflight check that `carina plan --check-iam` runs, which warns about missing AWS IAM actions before an apply. Return an empty list unless your provider manages AWS resources. `normalize_desired`, `normalize_state` and `hydrate_read_state` are optional hooks for providers whose API changes values on the way in or out. Their default implementations return the values unchanged, so implement them only when you need them.

Desired normalization has a three-way attribute contract. `DesiredResource::attribute` and `attributes` distinguish a `Known` value from an `Absent` attribute and a `Deferred` value that the host withheld until its dependency resolves. A deferred attribute is not absent: mutate only `Known` values, use `insert_absent` when adding a default, and return the opaque result from `finish`. This prevents normalization from replacing a deferred value by treating its missing map entry as user-authored absence.

## Next steps

[Testing](/providers/writing/testing/) tests these operations against a fake API, then end to end against a real account.
