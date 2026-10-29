# Creates That Reach the Cloud and Then Fail: Design

Tracking: carina-rs/carina#3909

## Problem

`Provider::create` returns `ProviderResult<CreateOutcome>`. Once a create has
reached the cloud, the only way to report a failure is a `ProviderError`, and
`ProviderError` has no slot for the resource it may have produced. The
executor maps every create `Err` to `BasicEffectResult::Failure { refresh:
None }` (the create arm in `carina-core/src/executor/basic.rs`). Nothing is
written to state, the cloud resource is orphaned, and the next apply plans a
fresh create that duplicates or conflicts with it.

carina#3913 closed the read-back half of this: a create whose post-create read
is not found or errors now returns `PartialSuccess` with the identifier the
create API returned. Everything else that can fail after the cloud accepted
the create still loses the resource. The issue started from one case (a create
that runs out of the host's wait budget) and was widened in its comments to
every "create reached the cloud, then `Err`".

### Inventory (provider `origin/main`: aws 5978120, awscc 99e17f8)

**aws: follow-up calls after the create API.** Almost every resource issues
more calls after the create API returns its identifier, and each one returns
`Err` with `?`:

| Resource | Calls that run after the create API and can fail |
|---|---|
| ec2.Vpc | CreateTags; ModifyVpcAttribute (DNS support); ModifyVpcAttribute (DNS hostnames) |
| ec2.Subnet | CreateTags; several ModifySubnetAttribute |
| ec2.InternetGateway | CreateTags; AttachInternetGateway |
| ec2.RouteTable | CreateTags; one CreateRoute per inline route, so a failure can leave some routes created |
| ec2.SecurityGroup, VpcEndpoint, VpnGateway, Eip, NatGateway | CreateTags |
| ec2.FlowLog | an `unsuccessful` item for one of several `resource_ids` fails or retries the whole call, so flow logs that were already created are orphaned or duplicated |
| logs.LogGroup | PutRetentionPolicy |
| s3.Bucket | PutBucketTagging |
| organizations.Account | ListParents; MoveAccount (after the account exists) |

For NatGateway, CreateTags runs inside the initial `create` call, before the
pending token exists.

**aws: host-polled creates (carina#3907).** The NatGateway, TransitGateway and
TransitGatewayAttachment tokens hold the cloud identifier for the whole wait.
The Organizations Account token holds only the `CreateAccount` request id,
because the account id is known only when the request finishes. A poll that
errors, a terminal `failed` state and an exhausted budget all drop the token.
The host then returns `ProviderError` (`over_budget` for the budget case), and
neither the identifier nor the request id gets out.

**awscc.**

- `CcRequestPhase::InFlight` holds only the CloudControl `RequestToken`. The
  `Identifier` that CreateResource and IN_PROGRESS events may carry is
  discarded.
- A `GetResourceRequestStatus` SDK error (throttling, network) returns `Err`,
  although the request is known to be running.
- When the budget runs out the token is dropped while the request keeps
  running in AWS.
- A FAILED event that carries an identifier is read back. When that read
  succeeds, the result is `CreateOutcome::Success`: the handler failure is
  dropped, and `merge_desired_attributes` copies the desired values for every
  property CloudControl did not return. The failed part is recorded as applied
  and is never re-sent.
- Whether `GetResource` finds a resource that is still being created depends
  on its handler. Many return NotFound until the physical resource is visible,
  so recording only an identifier would not stop the next plan from proposing
  a second create.
- `RequestToken` is an opaque request id with no credentials or desired state
  in it. `GetResourceRequestStatus` needs only that token plus credentials for
  the same account and region.

**Host-side writeback.** Even when a provider observes correctly, the host
fills in desired values for a created resource. Secret protection
(`PersistedAttributes::protect_serialized`) inserts a hash of the desired
value for every desired secret key, including keys the provider did not
return. `merge_write_only_attributes` copies every desired write-only value.
Both are right for a create that succeeded, and wrong for one whose
follow-up calls failed. (The same write-only merge after a failed or skipped
*update* is a separate path, carina#3923.)

**carina-provider-github** is a scaffold with no create implementation.

### Why this is a type-level problem

`ProviderError` is the only failure channel, so "nothing reached the cloud"
and "a resource exists but its configuration did not finish" have the same
type. Each provider would have to remember to turn the second case into a
success-shaped outcome; the aws inventory shows that it does not. awscc does
turn it into a success-shaped outcome, and that outcome then counts a failed
create as a success. Giving `ProviderError` an optional identifier would keep
the same convention, since every `?` would still produce the identifier-less
form.

The failure needs its own type, and it has to make the provider say which case
it is at every failure site. The value that reaches state has to record
whether it was observed or only desired. The current-state map has to keep
three things apart:

- applied: the provider identifier is known; for rows written from a created
  failure, the create observation records whether read-back found the resource;
- create in flight: the create was sent and has not finished;
- no current instance.

## Goals

- A create failure that leaves a resource in the cloud is recorded in state,
  for every provider, by the shape of the create API.
- A create still running when carina stops waiting for it is resumed by the
  next apply. No plan proposes a second create for it. The only automatic
  path that drops its record is the provider saying that no resource exists;
  the user can still forget it with a `removed` block.
- The executor treats both as failures: dependents are skipped and the exit
  code is non-zero.
- For a failed or resumed create, no value is recorded as applied unless the
  cloud is known to hold it: it was observed, or it was carried by the create
  request the cloud accepted. This holds for ordinary attributes, secrets and
  write-only attributes.

## Non-goals

- Duplicate creates from a create *send* that fails ambiguously (transport
  timeout) and is re-sent, in aws `retry_aws_operation` or the awscc
  `NotIssued` re-issue. No identifier or pending token exists at that point.
  This needs client tokens per API and is unchanged. A create API that reports
  success without returning an identifier is the same class: nothing can be
  recorded, and the provider returns `untracked` with that message. Two
  host-side cases also belong to it, because the guest's answer never arrives:
  - a WASM trap or the hard timeout during an initial `create` whose follow-up
    calls ran after the identifier was known;
  - a trap during a resume call that had already re-sent the create
    (`resend`). The next apply re-sends from the old envelope.
  - an unissued initial `pending` whose token cannot be encoded. The WIT create
    verdict has no better arm, so the guest reports it as `untracked`. The same
    failure after `resend` is `Resent(Untracked)` and the host reports `Ended`,
    unlike a trap during resend, which keeps the old envelope because no
    provider verdict arrived.
- Process termination between a provider's answer and the end-of-apply state
  save. This covers the second Ctrl+C, a kill, a CI timeout and
  `StateLockLost`. State is written once, after execution, so today such a
  termination also loses every *successful* create in the run. Saving
  incrementally conflicts with the carina#3907 lock heartbeat, which relies on
  no saves happening mid-run. It is a separate root cause, tracked in
  carina#3922.
- Update and delete that run out of budget keep today's behaviour (update
  queues a refresh; delete keeps the row and the next destroy re-issues it).
  Their tokens are not persisted.
- Adopting resources orphaned before this change.
- Writeback of failed or skipped updates and of `carina state refresh`
  (carina#3923).

## Decisions

Two choices had real trade-offs. Both were taken for the long-term,
type-safe, root-cause option.

**A create that is still running when carina stops waiting is resumed from a
persisted pending token.** Two alternatives were rejected:

- Marking the row "create in flight" and failing the plan when the resource
  reads NotFound is safe, but someone has to resolve it by hand. It also cannot
  record anything for awscc or the Organizations Account create, because their
  identifier is unknown at that point.
- Recording the identifier only, as Terraform does, keeps the duplicate create
  for awscc handlers that report NotFound while creating.

The cause is that carina loses the means to observe how its own operation
ends. The provider's pending token is that means. It already exists, it
already has to be free of secrets (carina#3907), and it covers the creates
whose identifier is not known yet.

**A create that finished in the cloud but failed a follow-up step converges
through the ordinary diff on the next plan.** The rejected alternative is to
taint the row and replace the resource. That would replace resources, such as
a VPC that has dependents, that the next update could finish. For the diff to
converge, the recorded row has to hold only what the cloud is known to have.
That is what was observed, plus the values carried by the create request the
cloud accepted (see "Rows built from an observation"). Everything else shows as a change on the
next plan, and the next apply sends it again. Write-only values that went out
in the create call are recorded, so a create-only write-only attribute does
not force a replacement.

## Design

### Shared create types (`carina-provider-protocol`)

The types that classify a create live in the protocol crate. Guest providers
(through the SDK), the builtin mock, the host and the executor then share one
definition. Every create receives an attempt value or an issued value from
its caller; providers do not construct one.

```rust
/// Handed to an initial create. Not Clone; every method consumes it.
pub struct CreateAttempt { /* private */ }
impl CreateAttempt {
    /// The provider knows no resource exists: nothing was sent, or the
    /// cloud rejected the request.
    pub fn untracked(self, error: ProviderError) -> CreateVerdict;
    /// The create request was accepted and the identifier is known.
    /// `sent` names the attributes whose values that request carried.
    pub fn issued(self, identifier: AppliedIdentifier, sent: Sent) -> IssuedCreate;
    /// Accepted, identifier not known yet.
    pub fn pending(self, token: PendingToken, sent: Sent, poll_after_secs: u32)
        -> CreateStep;
}

/// Handed to a poll of a pending create that has no identifier yet. The
/// `sent` set is the one the pending record carries; it cannot change.
pub struct PollAttempt { /* private */ }
impl PollAttempt {
    pub fn untracked(self, error: ProviderError) -> PollVerdict;
    pub fn issued(self, identifier: AppliedIdentifier) -> IssuedCreate;
    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> CreateStep;
    /// The provider cannot tell whether a resource exists.
    pub fn undetermined(self, error: ProviderError) -> PollVerdict;
}

pub struct IssuedCreate { /* identifier, sent */ }
impl IssuedCreate {
    pub fn identifier(&self) -> &AppliedIdentifier;
    /// Replace the create API's identifier with the provider's canonical one
    /// (awscc composite identifiers derived from the read-back). Allowed only
    /// on the way to a finished step or a verdict; a pending step keeps the
    /// identifier it was issued with.
    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingCreate;
    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> CreateStep;
    /// Success: every request attribute was sent. `attributes` may carry
    /// desired values the cloud does not return, as today; `partial` is the
    /// carina#3913 incomplete read-back.
    pub fn succeeded(self, attributes: Attributes) -> CreateStep;
    pub fn partial(self, attributes: Attributes, diagnostic: PartialReadDiagnostic)
        -> CreateStep;
    /// A follow-up step failed. The row is built from `read` and `sent`.
    pub fn failed(self, read: Observation, error: ProviderError) -> CreateVerdict;
    /// The cloud reports the resource as terminally failed or gone.
    pub fn discarded(self, error: ProviderError) -> CreateVerdict;
}
// `FinishingCreate` has `succeeded`, `partial`, `failed` and `discarded`,
// but no `pending`.

/// Handed to a resume of a recorded create that has no identifier.
pub struct ResumeAttempt { /* private */ }
impl ResumeAttempt {
    pub fn untracked(self, error: ProviderError) -> ResumeVerdict;
    pub fn undetermined(self, error: ProviderError) -> ResumeVerdict;
    pub fn issued(self, identifier: AppliedIdentifier) -> ResumedCreate;
    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> ResumeStep;
    /// The recorded request was never accepted; this call sends the current
    /// one. From here the create is an ordinary one, and its results are
    /// wrapped as `ResumeStep::Resent` / `ResumeVerdict::Resent`.
    pub fn resend(self) -> CreateAttempt;
}

/// Handed to a resume of a recorded create that has an identifier.
pub struct ResumedCreate { /* identifier */ }
impl ResumedCreate {
    pub fn identifier(&self) -> &AppliedIdentifier;
    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingResume;
    pub fn pending(self, token: PendingToken, poll_after_secs: u32) -> ResumeStep;
    /// Built from a found read and the recorded sent values; the current
    /// request may differ from the one the cloud received. A resumed create
    /// that reads NotFound is still running (`pending`) or gone
    /// (`discarded`), never completed.
    pub fn completed(self, read: FoundRead) -> ResumeStep;
    pub fn failed(self, read: Observation, error: ProviderError) -> ResumeVerdict;
    pub fn discarded(self, error: ProviderError) -> ResumeVerdict;
}

pub enum Observation { Read(ReadOutcome), ReadFailed(ProviderError) }
pub struct FoundRead(AppliedState);       // only from ReadOutcome::Found
pub enum Sent { AllRequestAttributes, Only(BTreeSet<String>) }
```

Steps (`CreateStep`, `ResumeStep`) are finished outcomes or pending values.
Verdicts (`CreateVerdict`, `PollVerdict`, `ResumeVerdict`) are failures the
provider has classified. `CreateVerdict` converts into `PollVerdict`, and a
create step or verdict produced after `resend` converts into the `Resent` arm
of the resume types. None of these types has a public constructor, and they
travel in channels separate from errors in the call itself (see Protocol
driver).

The SDK trait hands each call what it may use:

```rust
fn create(&self, id: &ResourceId, req: CreateRequest, attempt: CreateAttempt)
    -> Result<CreateStep<Self::CreateToken>, CreateVerdict>;
fn poll_create(&self, id: &ResourceId, req: CreateRequest, poll: Poll<Self::CreateToken>)
    -> Result<CreateStep<Self::CreateToken>, PollVerdict>;
fn resume_create(&self, id: &ResourceId, req: CreateRequest, resume: Resume<Self::CreateToken>)
    -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict>;

pub enum Poll<T>   { Unissued(PollAttempt, T),   Issued(IssuedCreate, T) }
pub enum Resume<T> { Unissued(ResumeAttempt, T), Issued(ResumedCreate, T) }
```

The glue picks the variant from the pending record's identifier. The shape
makes these rules hold:

- **Choosing a failure kind.** There is no `From<ProviderError>` into any of
  these types, so `?` on a `ProviderResult` does not compile inside a create.
  Every failure site has to name its case.
- **After `issued`.** The attempt has been moved, so `untracked` and
  `undetermined` can no longer be called. A follow-up failure is `failed`, and
  a cloud-reported terminal failure is `discarded`. A poll or resume of a
  record that has an identifier receives `Issued`, never an attempt, so it
  cannot give the identifier up.
- **Only through `issued`.** Outcomes that carry an identifier are reachable
  only through `issued` (and `canonicalize`), so they carry the identifier the
  provider declared. What the type cannot check is that the cloud API has
  actually run. A provider that keeps the identifier without calling `issued`
  has no way to return it.
- **"Cannot tell" belongs to polls.** On the initial call it would be the
  ambiguous-send non-goal, so `CreateAttempt` does not offer `undetermined`.
- **`PendingToken`.** It has a redacted `Debug` and no `Display`. Its raw text
  is reachable only through `expose_for_provider_call`, which the host's WIT
  conversion and the SDK glue use. It has serde so the state file can store
  it.

### Rows built from an observation

A `failed` create or resume produces an `ObservedCreate`. A successful resume
produces a `CompletedResume`, whose private `ObservedCreate` can only be built
from an `AppliedState`:

```rust
pub struct ObservedCreate {
    id: ResourceId,
    identifier: AppliedIdentifier,
    observation: CreateObservation,
    sent: SentRecord,     // the names, or the envelope's encoded values on resume
}

pub enum CreateObservation {
    Observed(AppliedState),
    Unobserved(UnobservedReason),
}

pub enum UnobservedReason {
    NotFound,
    ReadFailed(ProviderError),
}

pub struct CompletedResume(ObservedCreate); // private field and constructor
```

The provider-facing methods still accept the protocol `Observation`; core
converts it once when constructing `ObservedCreate`, so display and writeback
cannot classify the same read-back differently.

Writeback builds its row with `ResourceState::from_observation`. Masking and
recording are two separate decisions:

- **Which keys are masked** is decided exactly as today: by the desired
  resource's `Secret` marking, plus keys the previous row already hashed.
- **Which value is recorded** for each key is the observed value (masked if
  the key is secret), else the sent value, else nothing. A key with none of
  these is absent, so the next plan sends it.
  - The observed value is what the read returned. `Read(NotFound)` and
    `ReadFailed` give none; those attributes are `Unknown`
    (`UnknownReason::PostCreateReadIncomplete`, with the read error as the
    diagnostic).
  - The sent value is the desired value of a key in `sent`, encoded with the
    previous row's and the saved plan's secret hashes as today.
  - For a write-only key the read never returns a value, so the sent value
    decides.

So an awscc FAILED handler that did not apply a readable secret records the
hash of what it read. That hash does not match the desired value, so the
secret is sent again. A secret tag that a failed aws CreateTags never set is
absent. A create-only write-only attribute that the create API carried is
recorded, so it does not force a replacement.

`Sent` is how the provider says which desired values went out in the create
request itself. For awscc that is every property, since CloudControl sends
the whole model in one call. For aws it is the create API's own parameters;
the follow-up calls in the inventory are excluded. A create that succeeds
sent everything, and its writeback is unchanged from today.

### Pending records

There are two layers:

- **`PendingCreate`** (protocol) is what a provider returns with a pending
  step: the token, the identifier once issued, and the `sent` names.
- **`PendingEnvelope`** (state) is what carina persists: the token, the
  identifier, and the persisted encoding of the sent write-only and secret
  values, keyed by attribute name. Writeback produces the encoding, with the
  same previous and preferred hashes as any other row. Ordinary attributes
  are not stored, because a resumed row takes them from its observation.

### Core (`carina-core::provider`)

```rust
pub enum CreateFailure {
    Untracked(ProviderError),
    Created { created: ObservedCreate, error: ProviderError },
    InFlight { pending: InFlightCreate, error: ProviderError },
}

pub enum ResumeOutcome {
    Resumed(CompletedResume),
    Resent(CreateOutcome),       // after `resend`: an ordinary create
}
pub enum ResumeFailure {
    /// The provider's verdict that the create ended without a resource.
    Ended(ProviderError),
    Created { created: ObservedCreate, error: ProviderError },
    InFlight { pending: InFlightCreate, error: ProviderError },
}

/// A create recorded as running, bound to the resource it was issued for.
pub struct InFlightCreate {
    id: ResourceId,
    pending: PendingRecord,      // PendingCreate this run, or a loaded PendingEnvelope
    started_at: Timestamp,       // kept across re-pends
}
impl InFlightCreate {
    /// From the first pending step of a create sent in this run (an initial
    /// create, or one re-sent after `resend`).
    pub fn from_pending(id: ResourceId, pending: PendingCreate) -> Self;
    /// A resume that pends again: the new token replaces the old one, and the
    /// loaded envelope's identifier, encoded sent values and `started_at`
    /// are kept.
    pub fn repend(self, token: PendingToken) -> Self;
    // the state loader binds rows through a crate-private constructor;
    // `moved` re-keys through `rebind_for_move(self, to)`
}

pub struct ResumeCreateRequest { /* InFlightCreate + CreateRequest, same id */ }
impl ResumeCreateRequest {
    pub fn new(pending: InFlightCreate, request: CreateRequest)
        -> Result<Self, ResumeRoutingMismatch>;
    /// What a native provider resumes with: `Resume::Unissued` or
    /// `Resume::Issued` built from the record, the request, and a
    /// `Repend` handle that keeps the loaded envelope and `started_at`.
    pub fn into_parts(self) -> (Resume<PendingToken>, CreateRequest, Repend);
}

pub trait Provider {
    fn create(&self, id: &ResourceId, request: CreateRequest, attempt: CreateAttempt)
        -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>>;
    fn resume_create(&self, request: ResumeCreateRequest)
        -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>>;
    // read / update / delete unchanged
}
```

`CreateFailure`, `ResumeFailure` and `ObservedCreate` have crate-private
constructors. The public ways to build them are to convert a provider step or
verdict, to call `InFlightCreate::from_pending`, and to call `repend` (directly
or through `Repend`). A native `Provider` (the builtin
mock, test fakes) therefore classifies through the same attempt types. It
cannot write `map_err(CreateFailure::Untracked)` after a cloud call. A native
provider that defers drives its own steps with `drive_pending`, as the aws
and awscc native impls already do. On over budget it builds `InFlight` with
`from_pending` and the `over_budget` timeout as the error.

`ResumeFailure::Ended` exists only as the conversion of a provider's
`untracked` or `discarded` resume verdict. A provider `completed` step or
`created` resume verdict whose attributes fail host-side decoding becomes
`Created` with `ReadFailed`: the provider's verdict established an identifier,
while the failed decode means core cannot say that read-back observed the
resource. This is the same rule as an initial create result whose identifier
decoded before its attributes.
Every other host-side error during a resume becomes `InFlight` with the record
it started from. That includes:

- a routing error;
- a WASM trap or timeout;
- a decode failure;
- a pending step that drops or changes the recorded identifier.

So an in-flight row is never cleared automatically except by the provider.

A `ResumeRoutingMismatch` happens when the resource's address or provider
instance differs from the record, for example after `directives { provider =
... }` changed. It is a plan error, because polling one account and recording
the result under another leads to a duplicate create. `ProviderRouter` routes
`resume_create` on the request's single id.

### WIT (`carina-plugin-wit`, protocol v10)

```wit
// types.wit
variant observation { read(read-outcome), read-failed(provider-error) }
variant sent-attributes { all, only(list<string>) }
record pending-create {
    operation: pending-operation,
    identifier: option<applied-identifier>,
    sent: sent-attributes,
}
record created-failure {
    identifier: applied-identifier,      // canonical if canonicalized
    read: observation,
    sent: sent-attributes,
    error: provider-error,
}
variant create-step {
    success(applied-state),
    partial-success(create-partial-success),
    pending(pending-create),
}
variant create-verdict {
    untracked(provider-error),
    created(created-failure),
    discarded(provider-error),
}
variant poll-verdict {
    untracked(provider-error),
    created(created-failure),
    discarded(provider-error),
    undetermined(provider-error),
}
record resume-pending { operation: pending-operation, identifier: option<applied-identifier> }
record resumed-failure { identifier: applied-identifier, read: observation, error: provider-error }
variant resume-step {
    completed(applied-state),            // a found read; canonical identifier if canonicalized
    pending(resume-pending),
    resent(create-step),
}
variant resume-verdict {
    untracked(provider-error),
    created(resumed-failure),
    discarded(provider-error),
    undetermined(provider-error),
    resent(create-verdict),
}

// provider.wit
create:        func(id: resource-id, request: create-request)
               -> result<create-step, create-verdict>;
poll-create:   func(id: resource-id, request: create-request, pending: pending-create)
               -> result<create-step, poll-verdict>;
resume-create: func(id: resource-id, request: create-request, pending: resume-pending)
               -> result<resume-step, resume-verdict>;
```

A resumed create carries no `sent`; the host uses the envelope's encoded
values. A create re-sent after `resend` carries its own, under `resent`.
Verdicts and completed steps carry an observation, not a state. The host
builds the row through the same core constructors as the builtin mock, so a
guest cannot send desired values through them.

The guest glue maps a request or token that fails to decode in `poll-create`
or `resume-create` to `undetermined`, not to an ordinary `provider-error`.
Otherwise a token that a build cannot read would end the record.

The create token contract is tightened from "free of secrets" to "free of
secrets, and still decodable by later releases of the same provider".
Providers already tag their tokens by phase with serde, so adding a phase or
an optional field is compatible and renaming either is not. Update and delete
tokens are unaffected. `PROTOCOL_VERSION` and `MIN_SUPPORTED_PROTOCOL_VERSION`
move to 10.

### Protocol driver

`drive_pending` moves the pending value into each poll, so today it cannot
return the last pending value after a failure. Provider verdicts and call
failures also share one error type. Both change:

```rust
pub enum DriveOutcome<D, V, P> {
    Done(D),                         // a finished step
    Verdict(V),                      // a provider-classified failure
    OverBudget { last_pending: P },
}
pub enum DriveError<E, P> {
    Initial(E),                              // the first call failed
    Poll { error: E, last_pending: P },      // P: Clone; cloned before each poll
}
```

`DriveError` carries only call failures: a WASM trap, the hard timeout, or a
host-side conversion error. A provider's verdict always arrives as `Verdict`.

The SDK's JSON-RPC transport (`run`/`dispatch`) is deleted in Phase 1; the
CLI already rejects native provider binaries. Keeping it would need a second
encoding of every step, verdict and resumption.

### Host (`carina-plugin-host`)

`WasmProvider::create` and `resume_create` drive the operation with
`drive_pending`. The budget is a fresh `OperationConfig::budget(Create)` for
each invocation. `started_at` is set when the create is first sent and is
kept across invocations, but only for display.

`resume_create` drives `resume-create` until it returns something other than
`pending`. A `resent(pending(..))` switches the same drive to `poll-create`
under the remaining budget. From then on the pending record is the new
`pending-create`, with the `sent` of the current request.

| Drive result | `create` | `resume_create` |
|---|---|---|
| `Done(step)` | the outcome | `Resumed` (`completed`) or `Resent` |
| `Verdict(untracked \| discarded)` | `Untracked` | `Ended` |
| `Verdict(created)` | `Created` | `Created` |
| `Verdict(undetermined)` | `InFlight` with the last pending value | `InFlight` with the latest record |
| `Verdict(resent(v))` | — | `untracked` / `discarded` → `Ended`; `created` → `Created`; `undetermined` → `InFlight` with the post-resend record |
| `OverBudget` | `InFlight` | `InFlight` |
| `DriveError::Initial` | `Untracked` (ambiguous-send non-goal) | `InFlight` with the record it started from |
| `DriveError::Poll` | `InFlight` | `InFlight` |

The host enforces at the boundary what the SDK types enforce in the guest,
because the WIT itself admits more:

- On a record that has an identifier, an `untracked` or `undetermined`
  verdict is rejected. So is a pending step that drops the identifier or
  changes it. Each becomes `InFlight` with the record the call started from.
- A returned `pending-create` whose `sent` differs from the one passed in is
  rejected the same way. A changed
identifier is accepted only on a finished step or a verdict
(`canonicalize`). A provider that hits a transient error while polling (a
throttled describe, a throttled `GetResourceRequestStatus`) returns `pending`
again with a backoff, because any verdict ends the operation.

### Executor (`carina-core::executor`)

`BasicEffect` gains `ResumeCreate(ResumeCreateRequest)`. `AppliedStates`
values become a typed enum, so writeback cannot treat a failed or resumed
create as a finished one. Every current producer gets its own variant:

```rust
enum Applied {
    Complete(State),                   // Create Ok, Resent, Update Ok: as today
    Observed(ObservedCreate),          // Resumed, and CreateFailure / ResumeFailure::Created
    CreateInFlight(InFlightCreate),
    CreateEnded,                       // ResumeFailure::Ended
    Imported(State),                   // as today
    DataSource(State),                 // as today
    WaitSnapshot(State),               // as today; no row is written from it
}
```

| Result | Counted as | Dependents | `Applied` |
|---|---|---|---|
| Create `Ok`, Resume `Resent` | success / partial | run | `Complete` |
| Resume `Resumed` | success / partial | run | `Observed` |
| Create `Untracked` | failure | skipped | none |
| Resume `Ended` | failure | skipped | `CreateEnded` |
| `Created` (either) | failure | skipped | `Observed` |
| `InFlight` (either) | failure | skipped | `CreateInFlight` |
| ResumeCreate failing before dispatch (resolution error) or not run (dependency failed, cancelled) | failure / skipped | skipped | none |

A missing `Applied` entry for an in-flight address leaves its row unchanged
in writeback. `Created` and `InFlight` emit `EffectFailed` with an extra line
saying what was recorded:

- for an observed resource, "resource created; recorded in state, the next
  apply completes it";
- for an unobserved identifier, report the provider, identifier and read-back
  reason, then say it was "recorded in state so the next plan re-checks it";
- "create still in progress; the next apply resumes it".

Bindings and `wait` targets are not published for either, because their
dependents do not run.

### Managed-current map

`managed_current` changes from `HashMap<ResourceId, State>` to a map of:

```rust
pub struct AbsentState(State);       // private field
pub struct UnidentifiedState(State); // private field

pub enum Current {
    Absent(AbsentState),
    Present(PresentResourceState),
    CreateInFlight(InFlightCreate),
    MissingIdentifier(UnidentifiedState), // still becomes the existing plan error
}
```

`Current` is the only enum that gains `CreateInFlight`; managed loaders and
`Orphan` carry it directly instead of mirroring its variants in parallel
enums. `AbsentState`, `PresentResourceState`, and `UnidentifiedState` have
private fields and are minted by the state-classification boundary, so a
variant cannot contradict `State::exists` or provider-identifier validity.
Their read-only payload accessors do not flatten `Current`; `Current`
deliberately has no `state()` or `into_state()` accessor. An in-flight row
flattened to `State::not_found` is exactly the duplicate create. An in-flight
row is never refreshed, because there may be nothing to read. Every consumer
matches `Current` exhaustively:

- refresh: `refresh_resource_set` and `refresh_loaded_resource_with_retry`;
- `--refresh=false`, top level and loop children (wiring keeps the typed result
  returned by `build_state_for_resource`);
- destroy's state loading;
- `carina state` subcommands (`state.rs` loaders);
- the plan fixture loader;
- the saved plan's `CurrentStateEntry`. An in-flight entry is its own variant,
  not a non-existing `State`;
- the executor's managed input and result maps, the binding index's pre-apply
  inputs, and the differ's managed-current and orphan inputs;
- `materialize_moved_states`, including promotion of a moved orphan.

Provider normalization and hydration join managed current state with managed
orphans, but transform transient data-source state in a separate call. The two
key spaces can legitimately contain the same `ResourceId` after a managed
declaration becomes a `read`: the old managed row must be deleted while the new
data source is read. Anonymous managed-ID reconciliation still reserves active
data-source identities because wait-identifier and execution-result maps remain
keyed by `ResourceId` across both active declaration kinds.

### Writeback (`carina-cli` `state_writeback`)

`UpsertSource` gets one arm per `Applied` kind that writes a row, plus the
`Current` kinds, and `is_applied()` is deleted. `classify_replacement_outcome`
and `plan_displaced_identifier_deposes` switch on the source kind. Otherwise a
delete-first replacement whose create ends `Created` or `InFlight` would reach
`DeleteSucceededCreateAbsent` and remove the row.

| Source | Current slot | Old instance (CBD) | Old instance already deleted | Name overrides | Row built by |
|---|---|---|---|---|---|
| `Complete`, `Imported` | applied | deposed | current only | recorded (`Complete`) | today's constructor and write-only merge |
| `Observed` | applied | deposed | current only | recorded: the name was reported/sent; the observation says whether the object exists | `from_observation` |
| `CreateInFlight` from `from_pending` | in flight | deposed | in flight, no cleanup | recorded: the name was sent | a new envelope, encoded from the `sent` names and the request of this run |
| `CreateInFlight` from `repend` | in flight | unchanged | unchanged | unchanged | the loaded envelope with the new token; its encoded values are not touched |
| `CreateEnded` | cleared; `deposed` kept | unchanged | cleared | unchanged | — |
| `Current::Present` | applied | — | — | not recorded | as today (carina#3923 covers its write-only merge) |
| `Current::CreateInFlight` with no `Applied` | row unchanged, at its current address | — | — | unchanged | — |

The deposed snapshot that `planned_depose` builds for a CBD replacement is
unchanged. It describes the old instance, which is not affected by the new
create's failure.

**Deposed deletes.** An address whose current instance is in flight still
plans its deposed deletes. Each depends on the address's `ResumeCreate` and
runs only if that ends `Resumed` or `Resent`, so the old instance keeps
serving until the new one exists. Other deposed deletes are planned as today.

### State (`carina-state`, format v15)

`ResourceState`'s current instance becomes one typed field:

```rust
pub enum CurrentInstance {
    /// Deposed-only shell: no current instance.
    None,
    Applied { identifier: AppliedIdentifier, attributes: PersistedAttributes,
              write_only_attributes: Vec<String>, partial_read: Option<PartialReadMarker> },
    CreateInFlight { envelope: PendingEnvelope, started_at: Timestamp },
    /// A row that has attributes but no identifier, carried from v14 so the
    /// loader still reports `MissingProviderIdentifier`.
    MissingIdentifier { attributes: PersistedAttributes },
}
```

It replaces `pub identifier: Option<String>` and the attribute fields that
belong to the current instance, so "an identifier and a token" and "a token
and attributes" cannot be represented. Fields that describe the address stay
on `ResourceState`: binding, dependency bindings, directives, explicit fields,
prefixes, name overrides and protected. `is_deposed_only_shell` becomes a
check for `CurrentInstance::None` plus empty address fields. The v14 → v15
migration rewrites `identifier` and the attribute fields into `current`. No
v14 row is in flight.

On disk, an in-flight row is:

```json
{ "resource_type": "...", "identity": "...", "provider": "awscc",
  "current": { "kind": "create_in_flight",
               "envelope": { "token": "<opaque>", "identifier": null,
                             "write_only": { ... }, "secrets": { ... } },
               "started_at": "2026-10-23T08:00:00Z" },
  "deposed": [ ... ] }
```

The record holds no `ResourceId` or provider instance. The loader binds the
envelope to the row's own address and `directives.provider_instance`.

Each consumer handles an in-flight row explicitly:

- **plan / apply** (including `--refresh=false`): an address with a desired
  resource gets `Effect::ResumeCreate`. Plan does not call the provider,
  because a poll in a not-yet-accepted phase sends the create again. The
  display reads `↻ resume create <address> (in flight since <started_at>)`.
- **`moved`**: the `from` row loads as an in-flight orphan.
  `materialize_moved_states` re-keys it with `rebind_for_move` into
  `Current::CreateInFlight` at `to`, so the ResumeCreate and the row land at
  the new address. If the resume does not run, writeback writes the unchanged
  row at `to` before the move's cleanup of `from`.
- **orphan** (address removed from the configuration): after moves and
  `removed` blocks are processed, a remaining in-flight orphan is a plan error
  of a new kind, `PlanErrorKind::CreateInFlightOrphan`. The create cannot be
  resumed without a request, and deleting it needs an identifier that is not
  known yet. The message tells the user to restore the resource and apply, or
  to forget the row with a `removed` block, knowing that the cloud resource
  may then remain. `removed` suppresses this error the way it suppresses
  `MissingProviderIdentifier`, and it keeps deposed generations as it does
  today.
- **destroy**: a plan error naming the in-flight addresses, telling the user to
  run `apply` first. A delete needs an identifier when the plan is built, and
  none exists yet.
- **state refresh / list / lookup / export / remote_state**: the row is
  reported as in flight. It has no attributes, so references to it stay
  unresolved, as they do for a resource that has not been created.

### Saved plans

A saved `ResumeCreate` stores the address, not the token, and the saved
`CurrentStateEntry` marks the address as in flight. Under the apply lock,
saved-plan apply compares every address's in-flight status in state with the
plan's `CurrentStateEntry` and refuses on any difference, in either
direction. Without this, a saved `Create` could run over a row that a
concurrent apply left in flight. It then rebuilds each `InFlightCreate` from
the state row, so the loader stays the only source of a resumed envelope, and
tokens are not copied into plan files. This adds to the existing checks:
refusing a lineage mismatch and warning on a serial bump.

### State-bucket bootstrap

`create_checked_bootstrap_resource` creates the state bucket before any state
exists, and today only an auto-created bucket is written into state
(`with_managed_state_bucket`). A `Created` failure seeds the new state with
the observed row in the same way, then reports the error. An S3 bucket create
is never pending, so `InFlight` is unreachable there today. It is mapped to the
same seeding with an in-flight row, because the match is exhaustive.

### Providers

**awscc.**

- `create` returns `pending` with `Sent::AllRequestAttributes`.
- The CreateResource response and progress events call `issued` as soon as an
  `Identifier` appears. The read-back's composite identifier goes through
  `canonicalize`.
- FAILED with an identifier becomes `failed`, built from the read-back, never
  `Success`. FAILED without an identifier stays `untracked`, or is re-issued
  when retryable, as today.
- A `GetResourceRequestStatus` SDK error becomes `pending` with a backoff.
- On resume:
  - a record whose CloudControl request was in flight polls
    `GetResourceRequestStatus` with its `RequestToken` first, and reads the
    resource only after a terminal status. A recorded identifier does not
    mean the create has finished, and many handlers read NotFound until it
    has;
  - a record in the `NotIssued` phase calls `resend`;
  - an expired or unknown request token with no identifier is `undetermined`.
- The native `create_resource` driver moves to the new types.
- `main.rs` stops calling `carina_plugin_sdk::run`.

**aws.**

- Each create calls `issued` right after its create API returns the
  identifier, with `sent` naming that API's parameters. The follow-up calls in
  the inventory then fail as `failed`, with an observation from the shared
  read-back.
- The NAT gateway tags move after the pending step is built.
- FlowLog records every id it created. A response where only some ids
  succeeded becomes `failed` for those ids, without retrying the ones that
  succeeded.
- Describe errors while polling become `pending` with a backoff.
- Terminal cloud states (`failed`, `deleted`) are `discarded`.
- The Organizations Account create calls `issued` once the account id is
  known, so a failing `MoveAccount` records the account.
- `main.rs` stops calling `carina_plugin_sdk::run`.

**builtin mock / WASM mock.** Knobs to return each step and verdict, to stay
pending for N polls across invocations, to return a token that does not
decode, and to report a `sent` subset.

## Phases

Phase 1 is a carina-plugin-wit PR followed by a carina PR. Phase 2 is an aws
PR followed by an awscc PR pinned to it. They merge in that order.

1. **carina-plugin-wit + carina (protocol v10, state v15).**
   - Protocol: the attempt, issued and resume types and their verdicts;
     `PendingCreate`; `Observation`; `Sent`; `PendingToken`; the driver's
     `Verdict` channel and last-pending errors; JSON-RPC transport removed.
   - WIT: steps, verdicts and `resume-create`; SDK trait and glue, including
     decode failures.
   - Core: `CreateFailure`, `ObservedCreate`, the resume outcome and failure
     types, `InFlightCreate`, `ResumeCreateRequest`, the trait methods and
     `ProviderRouter`.
   - Host: the drive-result table and the switch after `resend`.
   - Executor: `BasicEffect::ResumeCreate` and the typed `Applied`.
   - The typed `Current` map at every site listed above.
   - CLI writeback: sources, replacement classification, `from_observation`,
     envelope encoding, and deposed-delete gating.
   - State: v15 `CurrentInstance` and migration; the new load and orphan
     variants, with the flattening accessors removed.
   - `ResumeCreate` in plan, display and saved plans; the destroy and
     `CreateInFlightOrphan` plan errors; bootstrap seeding; mocks.

   Tests:
   - each row of the executor table: the count, dependent skipping and the
     state written (CLI e2e with the builtin mock);
   - `Created` and `InFlight` under create_before_destroy and under a
     delete-first replace: the row survives with the expected current slot,
     and the old instance's deposed delete waits for the resume;
   - rows built from an observation:
     - observed attributes are recorded, and secret-marked ones are masked;
     - write-only values and secret hashes are recorded only for `sent` keys
       that were not observed;
     - a secret tag set by a failed follow-up call is absent, and the next
       plan sends it;
     - a readable secret that the cloud did not apply records the observed
       hash, and the next plan sends the desired one;
     - a create-only write-only attribute in `sent` causes no replacement;
   - a successful create, an import and a refreshed row are written exactly
     as before;
   - WASM mock: pending past the budget leaves an in-flight row. The next
     `carina plan` (also with `--refresh=false`) shows `resume create` and no
     `create`, for bound and anonymous resources, including an in-flight row
     that holds deposed generations. The next apply resumes it and writes an
     applied row, with `started_at` unchanged across re-pends;
   - each way the row must survive leaves it unchanged:
     - a ResumeCreate skipped because a dependency failed;
     - a resolution error or a routing error;
     - a poll trap, or a token the guest cannot decode;
     - `undetermined`;
     - a pending step that drops the identifier;
     - running out of budget again;
   - a resume verdict `untracked` clears the current slot and keeps `deposed`;
   - a resume of an issued record whose request is still running and whose
     read is NotFound stays in flight, and the next plan proposes no create;
   - a re-pend after the configuration changed keeps the old envelope's
     encoded values and `started_at`;
   - the host turns an `untracked` or `undetermined` verdict on an issued
     record, and a pending step with a changed `sent`, into `InFlight`;
   - a resume after the desired configuration changed records only observed
     values and the envelope's sent values. `resend` continues with
     `poll-create` and records the current request;
   - a composite identifier recorded through `canonicalize` on a finished
     step and on a resumed step;
   - an in-flight row removed from the configuration fails the plan with
     `CreateInFlightOrphan`, and a `removed` block clears it. Destroy fails on
     an in-flight row. A changed provider instance fails the plan;
   - `moved` over an in-flight row resumes at the new address, and keeps the
     row there when the resume does not run;
   - the v15 loader and the v14 migration, including a row with no
     identifier;
   - saved-plan apply refuses when an address's in-flight status changed in
     either direction;
   - the state-bucket bootstrap with a `Created` failure seeds the bucket row.
2. **aws + awscc rev bump.** Both providers move to v10 as described under
   Providers. Acceptance: the full aws and awscc suites and the multi-step
   tests pass. A provider-level test covers each inventory row: a failing
   follow-up call becomes a `created` verdict with the expected `sent` set,
   and an awscc FAILED event with an identifier becomes `created`. These tests
   use the winterbaume round-trip or the provider's mock clients.

## Alternatives considered

| Option | Why not |
|---|---|
| `ProviderError` with an optional identifier | Same convention as today: `?` produces the identifier-less form, so every post-create step has to remember it. |
| `CreateOutcome` gains a failed arm (`Ok` meaning failure) | The executor has to tell success-shaped values apart by a convention, and providers still need `ProviderError` for failures before the create. |
| Executor refreshes by identifier after a failed create | A create in progress can read NotFound, and a read error loses the identifier again. |
| State marker without the token; plan error on NotFound | Safe, but someone has to resolve it by hand, and it cannot cover creates whose identifier is not known yet (awscc, Organizations Account). |
| Identifier-only row, plan creates on NotFound (Terraform) | Leaves the duplicate create for awscc handlers. |
| Taint a created-then-failed row and replace it | Replaces resources that the next update could finish. Recording only observed and sent values gives the diff the same information. |
| Plan polls in-flight creates to show live status | A poll in a not-yet-accepted phase re-sends the create, so plan would change the cloud. |
| Destroy resumes, then deletes in the same run | Needs a delete whose identifier is produced during the run, a new effect shape used only here. Running `apply`, then `destroy`, does the same with existing effects. |
| Write-only and secrets of a failed create never recorded | A create-only write-only attribute would then force a replacement on the next plan, which is the taint outcome. |
| Save state when the first pending token arrives | Fixes only the in-flight part of process termination, and conflicts with the lock heartbeat; the general fix is incremental saving (carina#3922). |
| Host scans tokens for request secrets before persisting | A substring heuristic that misses config credentials and encoded forms; the token contract and its provider tests carry the guarantee. |
| Saved plan stores an envelope digest | Resuming the row's current envelope is correct even if a concurrent apply re-pended it; what matters is whether each address is in flight. |
