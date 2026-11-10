---
title: "Implementing Operations"
description: "How a Carina provider implements read, create, update and delete: identifiers, the create typestate, patches, long-running operations, errors and secrets."
---

This page walks through the operations of `widget`, an example provider for an imaginary API with one resource type. The API creates widgets asynchronously: a create call returns an operation ID, and the widget exists once that operation finishes. That is the shape most cloud APIs have, and it exercises most of the create typestate described in the [Overview](/providers/writing/overview/#short-calls-the-host-waits).

The provider holds an API client behind a trait, so the same code runs against the real client in the component and against a fake in [tests](/providers/writing/testing/). The examples use the SDK's token-derived continuation phases. They show the operations only; the provider still needs `info`, `schemas` and the rest of the trait shown in [Your First Provider](/providers/writing/getting-started/).

## The API client

```rust
//! Stand-in for the client of the API your provider manages.

#[derive(Clone)]
pub struct Widget {
    pub id: String,
    pub name: String,
    pub size: i64,
}

#[derive(Debug)]
pub enum ApiError {
    /// The API rejected the request; nothing was changed.
    Rejected(String),
    /// The resource does not exist.
    NotFound,
    /// The outcome is unknown: a timeout, a dropped connection, a 5xx.
    Unknown(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(message) | Self::Unknown(message) => f.write_str(message),
            Self::NotFound => f.write_str("widget not found"),
        }
    }
}
impl std::error::Error for ApiError {}

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

use std::collections::BTreeMap;

use carina_plugin_sdk::core::schema::ReferenceForms;
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::core::provider_text::{ProviderDiagnostic, ProviderText};
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never,
    Pending, PhasedToken, Poll, PollFinality, PollRequest, PollVerdict, Resume,
    ResumeStep, ResumeVerdict, Sent, UpdateOutcome,
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

    fn assignability_edges(&self) -> Vec<carina_plugin_sdk::core::schema::AssignabilityEdge> {
        Vec::new()
    }

    fn initialize(
        &mut self,
        _attrs: &BTreeMap<String, Value>,
    ) -> Result<ReferenceScope, ProviderError> {
        Ok(ReferenceScope::Unavailable)
    }

    // info, schemas, and the operations below
}
```

`CreateToken` is defined below. Updates and deletes finish in one call, so their token types are `Never`.

When an initialized instance knows its reference namespace, return
`ReferenceScope::available(partition, region, account)` from `initialize`.
It returns `ReferenceScopeError` when a value is empty or contains `{` or `}`;
map that error to a `ProviderError` for the trait method. Return
`ReferenceScope::Unavailable` when the namespace is not known; under
`Unavailable`, the provider's reference forms are inert and do not expand
short names during comparison.

The component exports one concrete type, for example `export_provider!(WidgetProvider<HttpApi>, http, egress: ["api.example.com"])` with your real client as `HttpApi`; the `http` form requires an egress declaration, and the SDK supplies its WASI bindings. `export_provider!` builds the provider with `Default`, so that type needs a `Default` implementation, written in the same crate as `WidgetProvider` (Rust's orphan rule rejects it in a separate binary crate), and the client should take its endpoint and credentials in `initialize`, from the `provider` block. See [Writing a Provider](/providers/writing/overview/#what-a-provider-is) for the declaration grammar and consent model.

## Helpers

```rust
// --- errors ---------------------------------------------------------------

fn error(kind: ProviderErrorKind, message: impl Into<String>) -> ProviderError {
    let message = message.into();
    let error = ProviderError::from_diagnostic(
        kind,
        ProviderDiagnostic::message(ProviderText::new(message)),
    );
    error.for_provider("widget")
}

fn api_error(operation: &str, err: ApiError) -> ProviderError {
    let kind = match &err {
        ApiError::Rejected(_) => ProviderErrorKind::InvalidInput,
        ApiError::NotFound => ProviderErrorKind::NotFound,
        ApiError::Unknown(_) => ProviderErrorKind::ApiError,
    };
    ProviderError::from_diagnostic(kind, err)
        .for_provider("widget")
        .with_operation(operation)
}

// --- attributes -----------------------------------------------------------

fn attributes(widget: &Widget) -> BTreeMap<String, Value> {
    BTreeMap::from([
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
    AppliedIdentifier::new(id).map_err(|source| {
        ProviderError::internal_caused_by("the API returned an empty widget ID", source)
            .for_provider("widget")
    })
}
```

`ProviderError` is the core opaque error, including in SDK authoring code. Message-only constructors accept strings or `ProviderText`. The five kinds are `InvalidInput`, `ApiError`, `NotFound`, `Timeout` and `Internal`. Attach cloud metadata with `with_operation`, `with_status`, `with_code` and `with_request_id`; Carina displays it on separate lines.

### Composing errors

Use the corresponding `*_caused_by(context, cause)` constructor to add context to a foreign error, a `ProviderError`, or a `ProviderDiagnostic`. Static strings convert to `ProviderContext`; wrap runtime context in `ProviderText`. This single operation preserves the complete cause, carried secret inventories and retry policy:

```rust
use carina_plugin_sdk::core::provider::ProviderError;
use carina_plugin_sdk::core::provider_text::{ProviderDiagnostic, ProviderText, Retryability};

let read = ProviderError::api_error_caused_by(
    "read failed", std::io::Error::other("service failure"),
).with_retryability(Retryability::Transient);
let context = ProviderText::new(format!("missing attributes: {}", "name"))
    .with_context("post-update read-back incomplete");
let error = ProviderError::api_error_caused_by(context, read);
assert!(error.is_transient());
assert!(error.detail().cause().unwrap().source_is::<std::io::Error>());

// Capture once when an observation and a verdict report the same failure.
let source = ProviderDiagnostic::new(error);
let observation = ProviderError::internal_caused_by("read-back failed", source.clone());
let verdict = ProviderError::internal_caused_by("read-back failed", source);
```

`from_diagnostic(kind, source)` adopts the complete diagnostic without adding context. Its text is rendered once. `ProviderDiagnostic::message(ProviderText)` creates an explicitly message-only reason. Errors and diagnostics do not convert to `ProviderText`; there is no `from_text` replacement-text API.

A cause-bearing error inherits retryability automatically. Foreign errors without a Carina error in their cause chain are `Unclassified`; attach SDK or provider policy to the composed error with `with_retryability(Retryability::Transient)`. `with_retryability(Retryability::Unclassified)` explicitly clears inherited retryability. The nearest Carina error that authors a verdict in the cause chain decides the class, and an explicit `Unclassified` is final; foreign frames and Carina pass-through wrappers are skipped. Structural validation wrappers inherit from their child; validation alternatives are `Unclassified` because their verdict is ambiguous. Use `DependencyBlocked` when deletion must wait for a dependent resource to go away; `is_delete_dependency()` checks only that typed class. A `Timeout`-kind error is never delete-retried, regardless of its retryability class. Throttling uses `Transient`. Message, cause and code never infer retryability. `is_transient()` is true only for `Transient`; the `Timeout` kind alone does not imply it.

Diagnostics share immutable causal storage when cloned. Each handle has its own text view, so binding one handle does not change another. There is one causal parent; combining independent typed causes is unsupported. SDK encoding flattens native causes and partial reasons to protocol-28 wire records. In wasm provider code, encoding returns plain wire records containing flattened text and metadata. Error records preserve the required typed retryability class; partial reasons remain strings. Neither record retains typed sources, and the wire decoder is private to the host. Keep the authored error or diagnostic when composing another failure; use the encoded record for transport.

Reference resolution returns only `ReferenceError::AccessDenied` or
`ReferenceError::InvalidReference`. For a temporary lookup failure, return
`InvalidReference` with the inner error classified using
`with_retryability(Retryability::Transient)`. Both variants stop plan-time
comparison; a temporary failure keeps the message
`reference resolution temporarily failed` based on its typed class.

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
    // Nothing has been sent yet, so a bad request is not-created.
    let (name, size) = match desired(&request.resource) {
        Ok(values) => values,
        Err(err) => return Err(attempt.not_created(err)),
    };
    let client_token = self.api.new_client_token();
    match self.api.start_create(&client_token, &name, size) {
        Ok(operation) => Ok(attempt.pending(
            CreateOperation::OperationId(operation),
            Sent::AllRequestAttributes,
            5,
        )),
        // The API said no: nothing exists.
        Err(err @ ApiError::Rejected(_)) => {
            Err(attempt.not_created(api_error("widget.Create", err)))
        }
        // The request may have been accepted. Keep the attempt and
        // retry it with the same client token in poll_create.
        Err(_) => Ok(attempt.pending(
            CreateOperation::Starting { client_token },
            Sent::AllRequestAttributes,
            5,
        )),
    }
}
```

The three outcomes follow the rules from the Overview. `not-created` means nothing was created: no request left the provider, or the cloud rejected it without creating anything. Both bad input found locally and an explicit rejection from the API qualify. The attempt is unissued because no identifier has been reported, even if a request was sent. When the call fails in a way that does not say whether the API accepted it, the provider cannot know whether a widget exists. Reporting `not-created` there could leave a widget that Carina has forgotten, so the provider returns `pending` with the client token and retries the call in `poll_create`.

`Sent::AllRequestAttributes` says that the request carried every attribute in the create request. If your API takes some attributes in a later call, name only the ones the create call carried, with `Sent::only([...])`.

The token is stored in state while the create is unfinished:

```rust
/// Operation continuations cannot contain a read-back marker.
#[derive(Debug, Serialize, Deserialize)]
pub enum CreateOperation {
    /// Retrying the same client token finds or starts the original create.
    Starting { client_token: String },
    /// The API accepted the request and returned an operation to follow.
    OperationId(String),
}

/// The host stores this token, so it holds no request values or secrets.
#[derive(Debug, Serialize, Deserialize)]
pub enum CreateToken {
    /// The mutation finished; only reading the widget back remains.
    ReadBack,
    /// Keep the existing Starting / OperationId JSON representation.
    #[serde(untagged)]
    Operation(CreateOperation),
}

impl PhasedToken for CreateToken {
    type Operation = CreateOperation;

    fn from_operation(operation: CreateOperation) -> Self {
        Self::Operation(operation)
    }

    fn into_operation(self) -> Result<CreateOperation, Self> {
        match self {
            Self::Operation(operation) => Ok(operation),
            Self::ReadBack => Err(self),
        }
    }

    fn operation(&self) -> Option<&CreateOperation> {
        match self {
            Self::Operation(operation) => Some(operation),
            Self::ReadBack => None,
        }
    }
}
```

`CreateOperation` holds only a client token or an operation ID. Before an identifier is known, `pending` and `preserve` take that payload directly, so provider code never checks a phase or unwraps a conversion. `Poll::Unissued` and `Resume::Unissued` also carry `CreateOperation`. Only SDK dispatch calls `into_operation` when decoding a full host token; a read-back token without an identifier ends polling with `READ_BACK_WITHOUT_IDENTIFIER`.

`PhasedToken::phase()` derives the phase from the borrowed `operation()` view: `Some` means operation, `None` means read-back. The borrowed and consuming views must agree, and `from_operation` must invert `into_operation`. Identifier-bearing handles accept the full token and derive their pending or preserved phase from it. The untagged operation variant retains the earlier token JSON form. Deriving `Serialize` and `Deserialize` needs `serde` with the `derive` feature: `serde = { version = "1", features = ["derive"] }`.

Within one apply, every poll receives the original request again, so the token does not need to repeat it. A create resumed by a later apply receives the request built from the configuration as it is then, which may have changed.

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
        Poll::Issued(issued, _token) => {
            return match self.read_found(id, issued.identifier()) {
                Ok(ReadOutcome::Found { state }) => Ok(issued.succeeded(state.attributes)),
                Ok(outcome) => Err(issued
                    .identifier_reported(
                        Observation::Read(outcome),
                        error(ProviderErrorKind::ApiError, "widget vanished after create"),
                    )
                    .into()),
                // The deadline poll must finish. Preserve the applied state
                // and tell Carina which value still needs a later refresh.
                Err(reason) if poll_request.finality == PollFinality::FinalAttempt => {
                    Ok(issued.partial(
                        BTreeMap::new(),
                        PartialReadDiagnostic::new(
                            carina_plugin_sdk::core::provider_text::ProviderDiagnostic::new(reason),
                            vec!["name".to_string(), "size".to_string(), "status".to_string()]
                                .try_into().expect("missing attributes are nonempty"),
                        ),
                    ))
                }
                // Keep the read-back continuation and surface the read error.
                Err(err) => Err(issued.preserve(CreateToken::ReadBack, 5, err).into()),
            };
        }
    };
    match token {
        CreateOperation::Starting { client_token } => {
            let (name, size) = match desired(&request.resource) {
                Ok(values) => values,
                Err(err) => {
                    return Err(attempt.preserve(
                        CreateOperation::Starting { client_token },
                        5,
                        err,
                    ));
                }
            };
            match self.api.start_create(&client_token, &name, size) {
                Ok(operation) => Ok(attempt.pending(CreateOperation::OperationId(operation), 5)),
                // A rejected retry does not prove the first call created
                // nothing: the API may refuse a reused client token, for
                // example because the configuration changed in between.
                Err(err @ ApiError::Rejected(_)) => {
                    Err(attempt.preserve(
                        CreateOperation::Starting { client_token },
                        5,
                        api_error("widget.Create", err),
                    ))
                }
                Err(_) => Ok(attempt.pending(CreateOperation::Starting { client_token }, 5)),
            }
        }
        CreateOperation::OperationId(operation) => match self.api.create_status(&operation) {
            Ok(CreateStatus::InProgress) => {
                Ok(attempt.pending(CreateOperation::OperationId(operation), 5))
            }
            Ok(CreateStatus::Done { id: widget_id }) => match identifier(&widget_id) {
                Ok(identifier) => {
                    let issued = attempt.issued(identifier);
                    if poll_request.finality == PollFinality::FinalAttempt {
                        // The operation completed at its deadline. Keep the
                        // identifier and settle without starting another poll.
                        Ok(issued.partial(
                            BTreeMap::new(),
                            PartialReadDiagnostic::new(
                            carina_plugin_sdk::core::provider_text::ProviderDiagnostic::message(
                                "widget read-back is still pending".into(),
                            ),
                            vec!["name".to_string(), "size".to_string(), "status".to_string()]
                                .try_into().expect("missing attributes are nonempty"),
                        ),
                        ))
                    } else {
                        Ok(issued.pending(CreateToken::ReadBack, 1))
                    }
                }
                Err(err) => Err(attempt.preserve(
                    CreateOperation::OperationId(operation),
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
                        .identifier_reported(observation, error(ProviderErrorKind::ApiError, reason))
                        .into())
                }
                Err(err) => Err(attempt.preserve(
                    CreateOperation::OperationId(operation),
                    5,
                    err,
                )),
            },
            Ok(CreateStatus::Failed { id: None, reason }) => {
                Err(attempt.not_created(error(ProviderErrorKind::ApiError, reason)))
            }
            // The status call failed; the operation is still there.
            Err(_) => Ok(attempt.pending(CreateOperation::OperationId(operation), 5)),
        },
    }
}
```

A `Starting` token retries the create with the same client token. Because the API treats the retry as the same request, it either starts the create now or returns the operation the first call started, so the provider does not create a second widget or claim one it did not create. That holds only while the API still remembers the client token; many APIs forget idempotency keys after a day or so.

A rejected retry is `preserve`, not `not-created`. The first call may have created the widget, and the API can refuse the reused token for reasons of its own, for example because the configuration changed before a later apply resumed the create. `preserve` carries the same continuation plus the error, so Carina keeps the attempt while surfacing why this poll stopped. Any other failure keeps the attempt pending.

When the operation finishes, the provider calls `attempt.issued(identifier)`. On an ordinary poll it returns `pending(CreateToken::ReadBack, ..)`, which records the identifier, starts the host's read-back budget, and makes the next poll go to the `Poll::Issued` arm. If the operation completes on the final attempt, the example settles with `partial` immediately, preserving the identifier for the next refresh.

`poll_request.finality` is `Continue` on ordinary polls and `FinalAttempt` on the one poll at the active deadline. Its rule follows the continuation the host sent: pending after a final attempt sent to the `Poll::Unissued` operation-phase continuation is over budget, even if that result starts read-back; pending in either phase after a final attempt sent to the `Poll::Issued` read-back continuation is a protocol violation. Returning from read-back to operation is always a violation. The example also returns `partial` when the issued read-back receives the final attempt and still cannot read the widget. Neither final-attempt branch has observed attributes, so both return an empty map and mark the unread attributes as missing.

`resume_create` receives the same poll request when Carina resumes a persisted continuation. SDK dispatch rejects only the backward phase pairing: host progress is read-back while the token is operation-phase. For create poll and resume this returns `unchanged` and keeps the previous continuation; update decoding rejects the same backward pairing. Operation progress with a read-back token is accepted when an identifier is available. This includes the first resume call: the host supplies `operating` because state has no phase metadata, and a saved read-back token can use the identifier-bearing handle to read from a fresh provider instance. Its next pending or preserved result reports read-back progress. A read-back token without an identifier ends create polling with `READ_BACK_WITHOUT_IDENTIFIER`.

On the wire, create and resume use `create-progress`: `operating(option<string>)` while the mutation is in progress, or `reading-back(string)` once it has completed. Read-back therefore always carries an identifier. Updates carry their phase in `pending-update`; their request already includes the identifier.

A create for which the provider reported an identifier is `identifier_reported` with that identifier, so Carina records the identifier plus only observed or sent values, and the next plan re-checks it. `identifier_reported` also takes an `Observation`: what the provider saw when it read the widget, either `Observation::Read` with the read outcome or `Observation::ReadFailed` with the read error. The observation passed to `identifier_reported` says whether the resource was found and supplies any observed state Carina can record. `identifier_reported` returns a `CreateVerdict<CreateToken>`, and `.into()` turns it into the `PollVerdict<CreateToken>` that `poll_create` returns.

`no_resource` is not used here. It reports the `no-resource` wire verdict when the provider knows no cloud resource remains, so Carina keeps no record of it. A create that failed without producing a widget is `not-created`. After an identifier is reported, `identifier_reported` retains it together with a read-back observation, which may find the resource, return not found, or fail. When the status call itself fails, the operation is still there, so the provider polls again. When a provider call has a usable continuation but also needs to report an error, `preserve` returns the error-bearing `in-flight` verdict. Provider code does not produce `unchanged`; guest transport glue reserves that wire arm for a request or token it cannot decode, when it has no newer continuation to return.

## resume_create

```rust
fn resume_create(
    &self,
    id: &ResourceId,
    _request: CreateRequest,
    resume: Resume<CreateToken>,
    _poll_request: PollRequest,
) -> Result<ResumeStep<CreateToken>, ResumeVerdict<CreateToken>> {
    match resume {
        // Carina was interrupted after the attempt started. Keep
        // polling where it left off.
        Resume::Unissued(attempt, token) => Ok(attempt.pending(token, 0)),
        Resume::Issued(resumed, token) => match self.read_found(id, resumed.identifier()) {
            Ok(ReadOutcome::Found { state }) => {
                Ok(resumed.completed(carina_plugin_sdk::FoundRead::from_applied_state(state)))
            }
            Ok(outcome) => Err(resumed.identifier_reported(
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
        carina_plugin_sdk::types::UpdateOutcome::Success {
            state: AppliedPersistedState {
                id: id.clone(),
                identifier: identifier.clone(),
                attributes: state.attributes,
            },
        },
    ))
}
```

Apply exactly the operations in the patch and nothing else. Check every operation before the first API call, so an operation the provider cannot apply does not leave the others half done. `request.from` is the last persisted state, and `request.state_after_patch()` returns it with the patch applied, which is usually what the update should report. The SDK's `UpdateOutcome` is either `Done` or `Pending`. `Done` wraps the SDK's `types::UpdateOutcome`, whose `Success` variant carries an `AppliedPersistedState`: the resource's identifier and its attributes after the update. `Add` and `Replace` both mean "set this value". `Remove` means the attribute was dropped from the configuration: unset it, or reset it to the API's default, and `state_after_patch` drops the key. `Reassert` carries an unchanged write-only value that the API needs again; send it like `Replace`. In `widget`, `size` is required and `name` is create-only, so no `Remove` and no change to `name` ever reaches `update`; the catch-all arm rejects anything the schema should have prevented.

`from` holds a secret value as a hash, never the secret itself. If the update needs a secret, it is in the patch.

If the update succeeded but reading the result back failed, return `PartialSuccess` with a `PartialReadDiagnostic`. Construct it with `PartialReadDiagnostic::new(ProviderDiagnostic::new(error), missing)`, where `missing` is a `NonEmptyMissingAttributes` obtained with `TryFrom<Vec<String>>`. Check the attributes before moving the error. Create, resumed-create and update partials retain the typed reason and its retryability until SDK encoding. Persistence and the wire keep the existing reason string. Incoming empty wire partials still decode as success.

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

`update` and `delete` can also return pending with a token and `poll_after_secs`. The host then calls `poll_update` or `poll_delete` with the original request until the operation finishes or the resource's time budget runs out. `poll_update` receives the same `PollRequest` finality as `poll_create`, and update continuations use `Pending::new(token, poll_after_secs)`. The update token implements `PhasedToken`, and `Pending::phase()` derives its phase from that token. There is no writable phase field or separate read-back constructor, so preserving or rebuilding the same continuation cannot change its phase. Delete uses the separate phase-less `PendingDelete` type, so it cannot express read-back. The token rules are the same as for create. A provider that finishes every update and delete in one call sets the token types to `Never`, as this one does.

## Secrets

Create, update and data-source input attributes reach the provider in plain text, including values the configuration wrapped in `secret(...)`. Desired normalization keeps secret leaves opaque, as described below. Carina masks them in plan output and stores only a hash in state, but inside the provider they are ordinary strings. Return API errors as typed causes so Carina can bind their text to the provider's input-secret inventory before rendering. Avoid copying raw attribute values into logs or pending tokens; tokens are written to state. Carina redacts carried context, causes and provider metadata together, including after cloning and nested composition.

## Data sources

A schema with `kind: SchemaKind::DataSource` declares a data source. The host calls `read_data_source` with the full resource, because a data source has no identifier and is looked up from its input attributes. Return a `State` with `exists` and the attributes you found. A provider without data sources returns an error, as `widget` does.

## Other trait methods

`reference_forms` is required; return `ReferenceForms::new()` when the provider declares no short-to-long reference forms. `assignability_edges` is also required; return `Vec::new()` when the provider declares no directional identity relations. `required_permissions` feeds the IAM preflight check that `carina plan --check-iam` runs, which warns about missing AWS IAM actions before an apply. Return an empty list unless your provider manages AWS resources. `normalize_desired`, `normalize_state` and `hydrate_read_state` are optional hooks for providers whose API changes values on the way in or out. Their default implementations return the values unchanged, so implement them only when you need them.

Desired normalization uses the same `DesiredView` on both routes. Each route stages its edits in an owner and applies them only after a successful `finish()`. A failed finish leaves the original resource unchanged.

`DesiredView::attribute` returns `None` for an absent attribute. A present `DesiredValue` is a `Known` scalar, a structural `List` or `Map`, or an opaque `Deferred(DeferredLeaf)`. References, unknowns and secrets are deferred leaves; known siblings inside the same container remain editable. Providers cannot construct, clone or inspect deferred leaves. Every received leaf must survive exactly once in its original resource and top-level attribute. Reordering within that attribute is allowed; dropping, duplicating or moving a leaf across that boundary fails at finish. Use `DesiredView::insert_absent` for top-level defaults and `DesiredMap::insert_absent` for nested defaults; an existing deferred leaf is occupied, not absent.

Use `DesiredInputs` for shared cross-field checks. Call this validator with `view.inputs()` during normalization or the infallible `DesiredInputs::from_provider_ready(request.resource())` during native create validation. Update receives desired values only through the checked patch, with no desired resource or validation view, per #2559. Create inputs include every host-added default and tag. These views use protocol map key order, and no raw-resource `DesiredInputs` constructor is public:

```rust
use carina_plugin_sdk::{DesiredInputs, DesiredValue};

fn required_together(inputs: DesiredInputs<'_>) -> bool {
    !matches!(inputs.attribute("kms_data_key_reuse_period_seconds"), Some(DesiredValue::Known(_)))
        || inputs.attribute("kms_master_key_id").is_some()
}
```

Secrets, references and unknowns appear as `Deferred` in the normalization projection. Provider-ready inputs have resolved references and known values; any remaining secret annotation stays opaque in the read-only view. The projection does not change the CRUD request: create/update bodies still receive actual secret values. A validator can defer checks that need an opaque value instead of treating it as absent.

`remove` returns the node, so it can be moved with `insert_absent` inside the same top-level attribute. `move_within` reorders a list element, including a whole map with deferred descendants:

```rust
use carina_plugin_sdk::{DesiredEditError, DesiredValueMut, DesiredView};

fn move_values(view: &mut DesiredView<'_>) -> Result<(), DesiredEditError> {
    if let Some(DesiredValueMut::Map(settings)) = view.attribute_mut("settings")
        && let Some(value) = settings.remove("old")
    {
        settings.insert_absent("new", value)?;
    }
    if let Some(DesiredValueMut::List(rules)) = view.attribute_mut("rules") {
        rules.move_within(1, 0)?;
    }
    Ok(())
}
```

Dropping a removed deferred leaf fails at finish with `MissingHandle`. Moving it into a different top-level attribute or resource fails with `ForeignHandle`.

For example, a certificate normalizer can lowercase known SANs and remove the known apex while retaining every deferred element:

```rust
use carina_plugin_sdk::{DesiredScalar, DesiredValueMut, DesiredView};

fn normalize_sans(view: &mut DesiredView<'_>, apex: &str) {
    if let Some(DesiredValueMut::List(sans)) = view.attribute_mut("subject_alternative_names") {
        sans.retain_known(|scalar| {
            let DesiredScalar::String(name) = scalar else { return true };
            let canonical = name.expose_plaintext().trim_end_matches('.').to_ascii_lowercase();
            let keep = canonical != apex;
            *name = canonical.into();
            keep
        });
    }
}
```

With apex `example.com`, a list shaped like `["EXAMPLE.COM", <deferred>, "WWW.EXAMPLE.COM"]` becomes `[<deferred>, "www.example.com"]`. `retain_known` never calls the closure on the deferred element, so it stays opaque and survives exactly once.

Wire the same normalizer into native resources with `NativeDesiredNormalization::from_core`, then `view()` and `finish()`:

```rust
use carina_plugin_sdk::{NativeDesiredNormalization, core::{provider::ProviderResult, resource::ProviderResource}};

fn normalize_native(resource: &mut ProviderResource) -> ProviderResult<()> {
    let mut desired = NativeDesiredNormalization::from_core(resource)?;
    normalize_sans(&mut desired.view(), "example.com");
    desired.finish()?;
    Ok(())
}
```

On WASM, use `into_normalization()` and the same function, then collect the checked results for `CarinaProvider::normalize_desired`:

```rust
use carina_plugin_sdk::{DesiredResource, NormalizedResource, types::ProviderError};

fn normalize_wasm(resource: DesiredResource) -> Result<NormalizedResource, ProviderError> {
    let mut desired = resource.into_normalization()?;
    normalize_sans(&mut desired.view(), "example.com");
    Ok(desired.finish()?)
}
```

Native builders and `finish()` return core `ProviderResult`, attaching the original resource id including its provider instance. Use `?` to propagate them; the terminal `ProviderError` retains the typed `NormalizationError` through `detail().cause()`, with nested conversion errors reachable through that cause's `source()`. Guest normalization errors convert through `?` to the core opaque error; SDK encoding flattens them into the protocol diagnostic channel.

## Next steps

[Testing](/providers/writing/testing/) tests these operations against a fake API, then end to end against a real account.
