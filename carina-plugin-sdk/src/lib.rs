//! Carina provider SDK for building WebAssembly component providers.
//!
//! Implement [`CarinaProvider`] and export it with [`export_provider!`].

pub use carina_core as core;
pub use carina_provider_protocol as protocol;
pub use carina_provider_protocol::types;

pub mod aws_credentials;

mod desired;
pub use desired::{
    DeferredLeaf, DesiredEditError, DesiredInputs, DesiredList, DesiredMap, DesiredNormalization,
    DesiredNormalizationError, DesiredResource, DesiredScalar, DesiredValue, DesiredValueMut,
    DesiredView, NativeDesiredNormalization, NormalizedResource,
};

#[cfg(target_arch = "wasm32")]
#[doc(hidden)]
pub mod __private;

/// The "Schemas" page of the provider guide on carina-rs.dev. Its Rust
/// examples run as doctests of this crate so they cannot drift from the SDK.
#[cfg(doctest)]
#[doc = include_str!("../../site/src/content/providers/writing/schemas.md")]
pub struct ProviderGuideSchemasPage;

/// Type-checks the provider guide's provider implementation on the host.
/// The wasm mock builds cover its target-gated `export_provider!` path.
#[cfg(doctest)]
#[doc = include_str!("../../site/src/content/providers/writing/getting-started.md")]
pub struct ProviderGuideGettingStartedPage;

#[cfg(any(target_arch = "wasm32", test))]
pub mod wasm_guest;

#[cfg(any(target_arch = "wasm32", test, feature = "test-utils"))]
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

use carina_core::schema::{
    ReferenceForms, ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use carina_provider_protocol::types::{
    AppliedIdentifier, AppliedState, CompletionValue, CreateRequest, DeleteRequest, PersistedState,
    PersistedValue, ProviderError, ProviderInfo, ReadOutcome, ReadRequest, ReferenceScope,
    Resource, ResourceId, State, TypeIdentity, UpdateRequest, Value,
};
use std::collections::{BTreeMap, HashMap};

/// Failure while projecting validated SDK schemas onto version-18 JSON.
#[derive(Debug)]
pub enum SchemaSerializationError {
    Convert(carina_core::protocol_convert::ProtocolConvertError),
    Json(serde_json::Error),
}

impl std::fmt::Display for SchemaSerializationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Convert(_error) => formatter.write_str("schema protocol conversion failed"),
            Self::Json(_error) => formatter.write_str("schema JSON serialization failed"),
        }
    }
}

impl std::error::Error for SchemaSerializationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Convert(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

/// Serialize one aggregate provider-schema witness through the core protocol
/// seam. Raw drafts or independently sourced tables cannot enter this API:
///
/// ```compile_fail
/// use carina_core::schema::ResourceSchema;
/// use carina_plugin_sdk::provider_schema_to_json;
///
/// let drafts = vec![ResourceSchema::new("test.Widget")];
/// let _ = provider_schema_to_json(&drafts);
/// ```
pub fn provider_schema_to_json(
    witness: &ValidatedProviderSchemaWitness,
) -> Result<String, SchemaSerializationError> {
    let protocol = carina_core::protocol_convert::provider_schema_to_protocol(witness)
        .map_err(SchemaSerializationError::Convert)?;
    serde_json::to_string(&protocol).map_err(SchemaSerializationError::Json)
}

/// Serialize one validated provider-config schema through the core seam.
/// A raw attribute-type map is intentionally not accepted:
///
/// ```compile_fail
/// use std::collections::BTreeMap;
/// use carina_core::schema::AttributeType;
/// use carina_plugin_sdk::provider_config_schema_to_json;
///
/// let draft = BTreeMap::<String, AttributeType>::new();
/// let _ = provider_config_schema_to_json(&draft);
/// ```
pub fn provider_config_schema_to_json(
    schema: &ValidatedConfigSchema,
) -> Result<String, SchemaSerializationError> {
    let protocol = carina_core::protocol_convert::config_schema_to_protocol(schema)
        .map_err(SchemaSerializationError::Convert)?;
    serde_json::to_string(&protocol).map_err(SchemaSerializationError::Json)
}

/// Schemas and validator dispatch captured once from an SDK provider.
///
/// The aggregate constructor rejects a semantic identity reused with a
/// different primitive scalar kind anywhere in the provider's resource or
/// config schemas.
pub struct ProviderSchemaCache {
    witness: ValidatedProviderSchemaWitness,
    custom_type_dispatch: carina_core::provider::CustomTypeDispatchTable,
}

impl ProviderSchemaCache {
    pub fn from_provider<P: CarinaProvider + ?Sized>(
        provider: &P,
    ) -> Result<Self, carina_core::schema::ProviderSchemaValidationError> {
        let resources = provider.schemas();
        let config = provider.provider_config_attribute_types();
        let reference_forms = provider.reference_forms();
        let witness = ValidatedProviderSchemaWitness::new(resources, config, reference_forms)?;
        let custom_type_dispatch =
            carina_core::provider::CustomTypeDispatchTable::from_provider_schema(&witness);
        Ok(Self {
            witness,
            custom_type_dispatch,
        })
    }

    pub fn schemas(&self) -> &[ValidatedResourceSchema] {
        self.witness.resources()
    }

    pub fn config(&self) -> &ValidatedConfigSchema {
        self.witness.config()
    }

    pub fn witness(&self) -> &ValidatedProviderSchemaWitness {
        &self.witness
    }
}

/// Dispatch one identity bridge call through the cached table.
///
/// The validated provider schemas determine the scalar kind. The version-18
/// guest ABI deliberately carries only the identity and canonical text.
pub fn dispatch_custom_type_validation(
    cache: &ProviderSchemaCache,
    identity: &TypeIdentity,
    canonical_text: &str,
) -> Result<(), String> {
    let core_identity = carina_core::schema::TypeIdentity::new(
        (!identity.provider.is_empty()).then(|| identity.provider.clone()),
        identity.segments.clone(),
        identity.kind.clone(),
    );
    cache
        .custom_type_dispatch
        .dispatch(&core_identity, canonical_text)
}

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
/// A create or update call, including each poll, is one short step that must
/// finish well under the host's per-call hard timeout; providers must never
/// run an in-call waiter. A poll may return another `Pending`. The host supplies the
/// original request on every poll, so `token` must contain only opaque
/// continuation metadata and must never contain request or secret data. A
/// token must also serialize as finite JSON. If an initial create has already
/// dispatched work but its identifier-less token cannot cross the WASM
/// boundary, the guest traps so the host can classify the attempt from its
/// per-call egress record instead of erasing it as untracked.
///
/// The phase comes only from the token; callers cannot supply a second phase:
///
/// ```compile_fail
/// use carina_plugin_sdk::{Pending, PendingPhase, PhasedToken};
///
/// fn disagree<T: PhasedToken>(token: T) -> Pending<T> {
///     Pending { token, poll_after_secs: 1, phase: PendingPhase::Operation }
/// }
/// ```
#[derive(Debug, Clone)]
pub struct Pending<T> {
    pub token: T,
    pub poll_after_secs: u32,
}

pub use carina_provider_protocol::types::{CreateProgress, PendingDelete, PendingToken, Sent};
pub use carina_provider_protocol::{PendingPhase, PollFinality, PollRequest};

/// A provider-owned continuation with a distinct operation-phase payload.
///
/// `from_operation` and `into_operation` must be inverses. `operation` returns
/// `Some` for exactly the variants accepted by `into_operation`; all other
/// variants are read-back continuations and require an identifier. The default
/// phase is derived from that borrowed view, so implementations need no
/// separate phase match.
///
/// Identifier-less create handles take the operation payload directly:
///
/// ```
/// use carina_plugin_sdk::{CreateAttempt, CreateStep, PhasedToken, Sent};
///
/// #[derive(Debug)]
/// enum Token {
///     Operating(String),
///     ReadBack,
/// }
///
/// impl PhasedToken for Token {
///     type Operation = String;
///
///     fn from_operation(operation: String) -> Self {
///         Self::Operating(operation)
///     }
///
///     fn into_operation(self) -> Result<String, Self> {
///         match self {
///             Self::Operating(operation) => Ok(operation),
///             Self::ReadBack => Err(self),
///         }
///     }
///
///     fn operation(&self) -> Option<&String> {
///         match self {
///             Self::Operating(operation) => Some(operation),
///             Self::ReadBack => None,
///         }
///     }
/// }
///
/// fn defer(attempt: CreateAttempt, operation_id: String) -> CreateStep<Token> {
///     attempt.pending(operation_id, Sent::AllRequestAttributes, 1)
/// }
/// ```
///
/// A full token, which could contain a read-back continuation, cannot be passed:
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateAttempt, CreateStep, PhasedToken, Sent};
/// fn pending<T: PhasedToken>(attempt: CreateAttempt, token: T) -> CreateStep<T> {
///     attempt.pending::<T>(token, Sent::AllRequestAttributes, 1)
/// }
/// ```
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateAttempt, CreateVerdict, PhasedToken, Sent, types::ProviderError};
/// fn preserve<T: PhasedToken>(attempt: CreateAttempt, token: T, error: ProviderError)
///     -> CreateVerdict<T>
/// {
///     attempt.preserve::<T>(token, Sent::AllRequestAttributes, 1, error)
/// }
/// ```
///
/// ```compile_fail
/// use carina_plugin_sdk::{PhasedToken, PollAttempt, CreateStep};
/// fn pending<T: PhasedToken>(attempt: PollAttempt, token: T) -> CreateStep<T> {
///     attempt.pending::<T>(token, 1)
/// }
/// ```
///
/// ```compile_fail
/// use carina_plugin_sdk::{PhasedToken, PollAttempt, PollVerdict, types::ProviderError};
/// fn preserve<T: PhasedToken>(attempt: PollAttempt, token: T, error: ProviderError)
///     -> PollVerdict<T>
/// {
///     attempt.preserve::<T>(token, 1, error)
/// }
/// ```
///
/// ```compile_fail
/// use carina_plugin_sdk::{PhasedToken, ResumeAttempt, ResumeStep};
/// fn pending<T: PhasedToken>(attempt: ResumeAttempt, token: T) -> ResumeStep<T> {
///     attempt.pending::<T>(token, 1)
/// }
/// ```
///
/// ```compile_fail
/// use carina_plugin_sdk::{PhasedToken, ResumeAttempt, ResumeVerdict, types::ProviderError};
/// fn preserve<T: PhasedToken>(attempt: ResumeAttempt, token: T, error: ProviderError)
///     -> ResumeVerdict<T>
/// {
///     attempt.preserve::<T>(token, 1, error)
/// }
/// ```
pub trait PhasedToken: Sized {
    /// The payload that can be carried without an identifier.
    type Operation;

    /// Wrap an operation payload without a runtime phase check.
    fn from_operation(operation: Self::Operation) -> Self;
    /// Narrow a host token, returning a read-back token unchanged.
    fn into_operation(self) -> Result<Self::Operation, Self>;
    /// Borrow the operation payload, or return `None` for read-back.
    fn operation(&self) -> Option<&Self::Operation>;

    /// Derive the continuation phase from its operation payload.
    fn phase(&self) -> PendingPhase {
        if self.operation().is_some() {
            PendingPhase::Operation
        } else {
            PendingPhase::ReadBack
        }
    }
}

impl<T: PhasedToken> Pending<T> {
    pub fn new(token: T, poll_after_secs: u32) -> Self {
        Self {
            token,
            poll_after_secs,
        }
    }

    pub fn phase(&self) -> PendingPhase {
        self.token.phase()
    }
}

/// Initial-create typestate minted only by the guest transport.
///
/// Provider implementations cannot forge an initial attempt:
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateAttempt, CreateVerdict, Sent};
/// use carina_plugin_sdk::types::{AppliedIdentifier, ProviderError};
///
/// fn lose_after_issue(
///     attempt: CreateAttempt,
///     identifier: AppliedIdentifier,
///     error: ProviderError,
/// ) -> CreateVerdict<()> {
///     let _issued = attempt.issued(identifier, Sent::AllRequestAttributes);
///     CreateAttempt::new().untracked(error)
/// }
/// ```
#[derive(Debug)]
pub struct CreateAttempt(());

impl CreateAttempt {
    fn new() -> Self {
        Self(())
    }

    pub fn untracked<T>(self, error: ProviderError) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::Untracked(
            error,
        ))
    }

    pub fn issued(self, identifier: AppliedIdentifier, sent: Sent) -> IssuedCreate {
        IssuedCreate { identifier, sent }
    }

    pub fn pending<T: PhasedToken>(
        self,
        token: T::Operation,
        sent: Sent,
        poll_after_secs: u32,
    ) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            carina_provider_protocol::types::PendingCreate {
                token: T::from_operation(token),
                progress: CreateProgress::Operating { identifier: None },
                sent,
                poll_after_secs,
            },
        ))
    }

    pub fn preserve<T: PhasedToken>(
        self,
        token: T::Operation,
        sent: Sent,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: carina_provider_protocol::types::PendingCreate {
                    token: T::from_operation(token),
                    progress: CreateProgress::Operating { identifier: None },
                    sent,
                    poll_after_secs,
                },
                error,
            },
        ))
    }
}

/// Poll typestate for a create that has not issued an identifier.
///
/// ```compile_fail
/// use carina_plugin_sdk::{PollAttempt, Sent};
///
/// let _ = PollAttempt::from_sent(Sent::AllRequestAttributes);
/// ```
///
/// Transport decode failures are classified by guest glue, not provider code:
///
/// ```compile_fail
/// use carina_plugin_sdk::{PollAttempt, PollVerdict};
/// use carina_plugin_sdk::types::ProviderError;
///
/// fn classify(attempt: PollAttempt, error: ProviderError) -> PollVerdict<()> {
///     attempt.undetermined(error)
/// }
/// ```
#[derive(Debug)]
pub struct PollAttempt {
    sent: Sent,
}

impl PollAttempt {
    fn from_sent(sent: Sent) -> Self {
        Self { sent }
    }

    pub fn untracked<T>(self, error: ProviderError) -> PollVerdict<T> {
        PollVerdict(carina_provider_protocol::types::PollVerdict::Untracked(
            error,
        ))
    }

    pub fn issued(self, identifier: AppliedIdentifier) -> IssuedCreate {
        IssuedCreate {
            identifier,
            sent: self.sent,
        }
    }

    pub fn pending<T: PhasedToken>(
        self,
        token: T::Operation,
        poll_after_secs: u32,
    ) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            carina_provider_protocol::types::PendingCreate {
                token: T::from_operation(token),
                progress: CreateProgress::Operating { identifier: None },
                sent: self.sent,
                poll_after_secs,
            },
        ))
    }

    pub fn preserve<T: PhasedToken>(
        self,
        token: T::Operation,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> PollVerdict<T> {
        PollVerdict(carina_provider_protocol::types::PollVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: carina_provider_protocol::types::PendingCreate {
                    token: T::from_operation(token),
                    progress: CreateProgress::Operating { identifier: None },
                    sent: self.sent,
                    poll_after_secs,
                },
                error,
            },
        ))
    }
}

/// Create typestate after the provider has observed a cloud identifier.
#[derive(Debug)]
pub struct IssuedCreate {
    identifier: AppliedIdentifier,
    sent: Sent,
}

impl IssuedCreate {
    fn from_parts(identifier: AppliedIdentifier, sent: Sent) -> Self {
        Self { identifier, sent }
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingCreate {
        FinishingCreate {
            identifier,
            sent: self.sent,
        }
    }

    pub fn pending<T: PhasedToken>(self, token: T, poll_after_secs: u32) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            self.into_pending(token, poll_after_secs),
        ))
    }

    fn into_pending<T: PhasedToken>(
        self,
        token: T,
        poll_after_secs: u32,
    ) -> carina_provider_protocol::types::PendingCreate<T> {
        carina_provider_protocol::types::PendingCreate {
            progress: match token.phase() {
                PendingPhase::Operation => CreateProgress::Operating {
                    identifier: Some(self.identifier),
                },
                PendingPhase::ReadBack => CreateProgress::ReadingBack {
                    identifier: self.identifier,
                },
            },
            token,
            sent: self.sent,
            poll_after_secs,
        }
    }

    pub fn preserve<T: PhasedToken>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: self.into_pending(token, poll_after_secs),
                error,
            },
        ))
    }

    pub fn succeeded<T>(self, attributes: BTreeMap<String, Value>) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Success {
            identifier: self.identifier,
            attributes,
        })
    }

    pub fn partial<T>(
        self,
        attributes: BTreeMap<String, Value>,
        diagnostic: carina_provider_protocol::types::PartialReadDiagnostic,
    ) -> CreateStep<T> {
        CreateStep(
            carina_provider_protocol::types::CreateStep::PartialSuccess {
                identifier: self.identifier,
                attributes,
                diagnostic,
            },
        )
    }

    pub fn failed<T>(
        self,
        observation: carina_provider_protocol::types::Observation<ReadOutcome, ProviderError>,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::Created(
            carina_provider_protocol::types::CreatedFailure {
                identifier: self.identifier,
                observation,
                sent: self.sent,
                error,
            },
        ))
    }

    pub fn discarded<T>(self, error: ProviderError) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::Discarded(
            error,
        ))
    }
}

/// Canonicalized issued create. Canonicalization deliberately removes the
/// ability to return to pending.
#[derive(Debug)]
pub struct FinishingCreate {
    identifier: AppliedIdentifier,
    sent: Sent,
}

impl FinishingCreate {
    pub fn succeeded<T>(self, attributes: BTreeMap<String, Value>) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Success {
            identifier: self.identifier,
            attributes,
        })
    }

    pub fn partial<T>(
        self,
        attributes: BTreeMap<String, Value>,
        diagnostic: carina_provider_protocol::types::PartialReadDiagnostic,
    ) -> CreateStep<T> {
        CreateStep(
            carina_provider_protocol::types::CreateStep::PartialSuccess {
                identifier: self.identifier,
                attributes,
                diagnostic,
            },
        )
    }

    pub fn failed<T>(
        self,
        observation: carina_provider_protocol::types::Observation<ReadOutcome, ProviderError>,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::Created(
            carina_provider_protocol::types::CreatedFailure {
                identifier: self.identifier,
                observation,
                sent: self.sent,
                error,
            },
        ))
    }

    pub fn discarded<T>(self, error: ProviderError) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::Discarded(
            error,
        ))
    }
}

/// Resume typestate for a persisted create without an identifier.
///
/// ```compile_fail
/// use carina_plugin_sdk::ResumeAttempt;
///
/// let _ = ResumeAttempt::new();
/// ```
#[derive(Debug)]
pub struct ResumeAttempt(());

impl ResumeAttempt {
    fn new() -> Self {
        Self(())
    }

    pub fn untracked<T>(self, error: ProviderError) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::Untracked(
            error,
        ))
    }

    pub fn issued(self, identifier: AppliedIdentifier) -> ResumedCreate {
        ResumedCreate { identifier }
    }

    pub fn pending<T: PhasedToken>(
        self,
        token: T::Operation,
        poll_after_secs: u32,
    ) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Pending(
            carina_provider_protocol::types::ResumePending {
                token: T::from_operation(token),
                progress: CreateProgress::Operating { identifier: None },
                poll_after_secs,
            },
        ))
    }

    pub fn preserve<T: PhasedToken>(
        self,
        token: T::Operation,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::InFlight(
            carina_provider_protocol::types::ResumeInFlight {
                pending: carina_provider_protocol::types::ResumePending {
                    token: T::from_operation(token),
                    progress: CreateProgress::Operating { identifier: None },
                    poll_after_secs,
                },
                error,
            },
        ))
    }

    pub fn resend(self) -> CreateAttempt {
        CreateAttempt::new()
    }
}

/// A successful read with the absence case eliminated. It can only be
/// obtained by consuming [`ReadOutcome::Found`].
///
/// ```compile_fail
/// use carina_plugin_sdk::FoundRead;
/// use std::collections::BTreeMap;
///
/// let _ = FoundRead { attributes: BTreeMap::new() };
/// ```
#[derive(Debug)]
pub struct FoundRead {
    attributes: BTreeMap<String, Value>,
}

impl FoundRead {
    /// Narrow an already-found read by taking its success payload directly.
    pub fn from_applied_state(state: AppliedState) -> Self {
        Self {
            attributes: state.attributes,
        }
    }
}

impl TryFrom<ReadOutcome> for FoundRead {
    type Error = ReadOutcome;

    fn try_from(outcome: ReadOutcome) -> Result<Self, Self::Error> {
        match outcome {
            ReadOutcome::Found { state } => Ok(Self::from_applied_state(state)),
            ReadOutcome::NotFound { id } => Err(ReadOutcome::NotFound { id }),
        }
    }
}

/// Resume typestate for a persisted create with an identifier.
#[derive(Debug)]
pub struct ResumedCreate {
    identifier: AppliedIdentifier,
}

impl ResumedCreate {
    fn new(identifier: AppliedIdentifier) -> Self {
        Self { identifier }
    }

    pub fn identifier(&self) -> &AppliedIdentifier {
        &self.identifier
    }

    pub fn canonicalize(self, identifier: AppliedIdentifier) -> FinishingResume {
        FinishingResume { identifier }
    }

    pub fn pending<T: PhasedToken>(self, token: T, poll_after_secs: u32) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Pending(
            self.into_pending(token, poll_after_secs),
        ))
    }

    fn into_pending<T: PhasedToken>(
        self,
        token: T,
        poll_after_secs: u32,
    ) -> carina_provider_protocol::types::ResumePending<T> {
        carina_provider_protocol::types::ResumePending {
            progress: match token.phase() {
                PendingPhase::Operation => CreateProgress::Operating {
                    identifier: Some(self.identifier),
                },
                PendingPhase::ReadBack => CreateProgress::ReadingBack {
                    identifier: self.identifier,
                },
            },
            token,
            poll_after_secs,
        }
    }

    pub fn preserve<T: PhasedToken>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::InFlight(
            carina_provider_protocol::types::ResumeInFlight {
                pending: self.into_pending(token, poll_after_secs),
                error,
            },
        ))
    }

    pub fn completed<T>(self, read: FoundRead) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Completed {
            identifier: self.identifier,
            attributes: read.attributes,
        })
    }

    pub fn partial<T>(
        self,
        attributes: BTreeMap<String, Value>,
        diagnostic: carina_provider_protocol::types::PartialReadDiagnostic,
    ) -> ResumeStep<T> {
        ResumeStep(
            carina_provider_protocol::types::ResumeStep::PartialSuccess {
                identifier: self.identifier,
                attributes,
                diagnostic,
            },
        )
    }

    pub fn failed<T>(
        self,
        observation: carina_provider_protocol::types::Observation<ReadOutcome, ProviderError>,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::Created(
            carina_provider_protocol::types::ResumedFailure {
                identifier: self.identifier,
                observation,
                error,
            },
        ))
    }

    pub fn discarded<T>(self, error: ProviderError) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::Discarded(
            error,
        ))
    }
}

/// Canonicalized resumed create. It cannot return to pending.
#[derive(Debug)]
pub struct FinishingResume {
    identifier: AppliedIdentifier,
}

impl FinishingResume {
    pub fn completed<T>(self, read: FoundRead) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Completed {
            identifier: self.identifier,
            attributes: read.attributes,
        })
    }

    pub fn partial<T>(
        self,
        attributes: BTreeMap<String, Value>,
        diagnostic: carina_provider_protocol::types::PartialReadDiagnostic,
    ) -> ResumeStep<T> {
        ResumeStep(
            carina_provider_protocol::types::ResumeStep::PartialSuccess {
                identifier: self.identifier,
                attributes,
                diagnostic,
            },
        )
    }

    pub fn failed<T>(
        self,
        observation: carina_provider_protocol::types::Observation<ReadOutcome, ProviderError>,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::Created(
            carina_provider_protocol::types::ResumedFailure {
                identifier: self.identifier,
                observation,
                error,
            },
        ))
    }

    pub fn discarded<T>(self, error: ProviderError) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::Discarded(
            error,
        ))
    }
}

/// Typestate and provider-owned token passed to a create poll.
#[derive(Debug)]
pub enum Poll<T: PhasedToken> {
    Unissued(PollAttempt, T::Operation),
    Issued(IssuedCreate, T),
}

/// Typestate and provider-owned token passed to a persisted create resume.
#[derive(Debug)]
pub enum Resume<T: PhasedToken> {
    Unissued(ResumeAttempt, T::Operation),
    Issued(ResumedCreate, T),
}

pub type ProtocolCreateStep<T> = carina_provider_protocol::types::CreateStep<
    BTreeMap<String, Value>,
    carina_provider_protocol::types::PartialReadDiagnostic,
    T,
>;

/// SDK result of one create step. Its private field keeps identifier-bearing
/// outcomes reachable only through an issued typestate handle.
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateStep, types::{AppliedIdentifier, Value}};
/// use std::collections::BTreeMap;
///
/// fn bypass(identifier: AppliedIdentifier) -> CreateStep<()> {
///     CreateStep::Success {
///         identifier,
///         attributes: BTreeMap::<String, Value>::new(),
///     }
/// }
/// ```
#[derive(Debug)]
pub struct CreateStep<T>(ProtocolCreateStep<T>);

impl<T> CreateStep<T> {
    fn into_protocol(self) -> ProtocolCreateStep<T> {
        self.0
    }
}

pub type ProtocolCreateVerdict<T> =
    carina_provider_protocol::types::CreateVerdict<ReadOutcome, T, ProviderError>;
pub type ProtocolPollVerdict<T> =
    carina_provider_protocol::types::PollVerdict<ReadOutcome, T, ProviderError>;
pub type ProtocolResumeVerdict<T> =
    carina_provider_protocol::types::ResumeVerdict<ReadOutcome, T, ProviderError>;

/// SDK verdict from the initial create call.
///
/// Plain operation errors do not convert into a create verdict, so provider
/// code must explicitly classify every failure through its typestate handle.
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateVerdict, types::{ProviderError, ProviderErrorKind}};
///
/// fn classify() -> Result<(), CreateVerdict<()>> {
///     Err::<(), ProviderError>(ProviderError {
///         kind: ProviderErrorKind::Internal,
///         message: "failed".to_string(),
///         resource_id: None,
///         cause: None,
///         provider_name: None,
///         operation: None,
///         status: None,
///         code: None,
///         request_id: None,
///     })?;
///     Ok(())
/// }
/// ```
///
/// A raw verdict variant cannot bypass the SDK facade either:
///
/// ```compile_fail
/// use carina_plugin_sdk::{CreateVerdict, types::ProviderError};
///
/// fn bypass(error: ProviderError) -> CreateVerdict<()> {
///     CreateVerdict::Untracked(error)
/// }
/// ```
#[derive(Debug)]
pub struct CreateVerdict<T>(ProtocolCreateVerdict<T>);

impl<T> CreateVerdict<T> {
    fn into_protocol(self) -> ProtocolCreateVerdict<T> {
        self.0
    }
}

/// SDK verdict from polling a create.
///
/// ```compile_fail
/// use carina_plugin_sdk::{PollVerdict, ProtocolPollVerdict};
///
/// fn bypass(raw: ProtocolPollVerdict<()>) -> PollVerdict<()> {
///     PollVerdict(raw)
/// }
/// ```
#[derive(Debug)]
pub struct PollVerdict<T>(ProtocolPollVerdict<T>);

impl<T> PollVerdict<T> {
    fn into_protocol(self) -> ProtocolPollVerdict<T> {
        self.0
    }
}

impl<T> From<CreateVerdict<T>> for PollVerdict<T> {
    fn from(verdict: CreateVerdict<T>) -> Self {
        Self(verdict.0.into())
    }
}

pub type ProtocolResumeStep<T> = carina_provider_protocol::types::ResumeStep<
    BTreeMap<String, Value>,
    carina_provider_protocol::types::PartialReadDiagnostic,
    T,
>;

/// SDK result of one resumed-create step.
///
/// ```compile_fail
/// use carina_plugin_sdk::{ProtocolResumeStep, ResumeStep};
///
/// fn bypass(raw: ProtocolResumeStep<()>) -> ResumeStep<()> {
///     ResumeStep(raw)
/// }
/// ```
#[derive(Debug)]
pub struct ResumeStep<T>(ProtocolResumeStep<T>);

impl<T> ResumeStep<T> {
    fn into_protocol(self) -> ProtocolResumeStep<T> {
        self.0
    }
}

impl<T> From<CreateStep<T>> for ResumeStep<T> {
    fn from(step: CreateStep<T>) -> Self {
        Self(step.0.into())
    }
}

/// SDK verdict from resuming a persisted create.
///
/// ```compile_fail
/// use carina_plugin_sdk::{ProtocolResumeVerdict, ResumeVerdict};
///
/// fn bypass(raw: ProtocolResumeVerdict<()>) -> ResumeVerdict<()> {
///     ResumeVerdict(raw)
/// }
/// ```
#[derive(Debug)]
pub struct ResumeVerdict<T>(ProtocolResumeVerdict<T>);

impl<T> ResumeVerdict<T> {
    fn into_protocol(self) -> ProtocolResumeVerdict<T> {
        self.0
    }
}

impl<T> From<CreateVerdict<T>> for ResumeVerdict<T> {
    fn from(verdict: CreateVerdict<T>) -> Self {
        Self(verdict.0.into())
    }
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
    Pending(PendingDelete<T>),
}

/// Token type for an operation kind that this provider can never defer.
#[derive(Debug)]
pub enum Never {}

impl PhasedToken for Never {
    type Operation = Self;

    fn from_operation(operation: Self::Operation) -> Self {
        match operation {}
    }

    fn into_operation(self) -> Result<Self::Operation, Self> {
        match self {}
    }

    fn operation(&self) -> Option<&Self::Operation> {
        match *self {}
    }
}

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
    type CreateToken: PhasedToken + serde::Serialize + serde::de::DeserializeOwned;
    type UpdateToken: PhasedToken + serde::Serialize + serde::de::DeserializeOwned;
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
    fn schemas(&self) -> Vec<ValidatedResourceSchema>;

    /// Return provider-wide reference forms keyed by refined String identity.
    /// Resource schemas, config, and this table are admitted together before
    /// any schema metadata is emitted.
    fn reference_forms(&self) -> ReferenceForms;

    /// Return the types of the provider block's configuration attributes
    /// (e.g., `region`). The host uses these to validate attributes
    /// against their declared types *before* calling [`validate_config`].
    /// This keeps format validation on the host side so fixes in
    /// `carina-core` take effect without rebuilding provider binaries.
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema;

    /// Validate provider-specific configuration semantics not expressible
    /// in the attribute type schema. Host-side type validation has
    /// already run by the time this is called.
    fn validate_config(&self, attrs: &BTreeMap<String, Value>) -> Result<(), String>;

    /// Initialize the provider with configuration.
    /// Called once before any CRUD operations.
    fn initialize(&mut self, attrs: &BTreeMap<String, Value>) -> Result<ReferenceScope, String>;

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
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>>;

    /// Perform one short poll step for a pending create operation.
    ///
    /// The host passes the original request on every poll. This method may
    /// return a pending step again and must not wait in-call. A
    /// [`PollFinality::FinalAttempt`] sent to a read-back continuation must
    /// finish with success, partial success, or an error; pending in either
    /// phase is a protocol violation. Pending after a final attempt sent to an
    /// operation continuation is instead reported as over budget, even if the
    /// response moves to read-back. A read-back continuation must never regress
    /// to operation phase.
    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        poll: Poll<Self::CreateToken>,
        poll_request: PollRequest,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>>;

    /// Continue a create operation loaded from state. `poll_request` has the
    /// same host-owned finality and monotonic read-back semantics as
    /// [`CarinaProvider::poll_create`]. SDK dispatch rejects a token that would
    /// regress the host's read-back progress to operation. The first call uses
    /// operation progress because state retains no scheduling metadata. A saved
    /// read-back token with an identifier can resume through an issued handle;
    /// its next pending or preserved result reports read-back progress.
    fn resume_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        resume: Resume<Self::CreateToken>,
        poll_request: PollRequest,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>>;

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
    /// in-call. A [`PollFinality::FinalAttempt`] sent to a read-back
    /// continuation must not return pending in either phase. Pending after a
    /// final attempt sent to an operation continuation is instead reported as
    /// over budget, even if the response moves to read-back. Read-back must
    /// never regress to operation phase.
    fn poll_update(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: UpdateRequest,
        pending: Pending<Self::UpdateToken>,
        poll_request: PollRequest,
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
    /// in-call. [`PendingDelete`] deliberately has no phase because delete has
    /// no post-apply read-back step.
    fn poll_delete(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        request: DeleteRequest,
        pending: PendingDelete<Self::DeleteToken>,
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

    /// Normalize desired resources through a structural [`DesiredView`].
    ///
    /// Consume each [`DesiredResource`] with [`DesiredResource::into_normalization`],
    /// edit known leaves through [`DesiredNormalization::view`], and return its
    /// checked [`DesiredNormalization::finish`]. Lists/maps remain visible beside
    /// opaque secret/reference/unknown leaves. Every deferred leaf must survive
    /// exactly once within its original resource and top-level attribute.
    /// Validate cross-field inputs with [`DesiredView::inputs`] here, or
    /// [`DesiredInputs::from_core`] during native create/update validation.
    /// That projection keeps secrets/references deferred; CRUD bodies still get
    /// real secret values. Move nodes with [`DesiredMap::remove`] followed by
    /// [`DesiredMap::insert_absent`], or reorder with [`DesiredList::move_within`].
    /// Each deferred leaf must remain in its original top-level attribute.
    ///
    /// Both routes call the same function and propagate errors with `?`. Native
    /// builders/finish return core provider errors with the original resource id
    /// and provider instance; the typed normalization error remains accessible
    /// through `detail().cause()`. Guest errors use the protocol error type.
    ///
    /// ```
    /// use carina_plugin_sdk::{DesiredResource, DesiredView, NativeDesiredNormalization,
    ///     NormalizedResource, types::ProviderError};
    /// use carina_plugin_sdk::core::{provider::ProviderResult, resource::ProviderResource};
    /// # fn shared(_: &mut DesiredView<'_>) {}
    /// fn native(resource: &mut ProviderResource) -> ProviderResult<()> {
    ///     let mut desired = NativeDesiredNormalization::from_core(resource)?;
    ///     shared(&mut desired.view());
    ///     desired.finish()?;
    ///     Ok(())
    /// }
    /// fn wasm(resource: DesiredResource) -> Result<NormalizedResource, ProviderError> {
    ///     let mut desired = resource.into_normalization()?;
    ///     shared(&mut desired.view());
    ///     Ok(desired.finish()?)
    /// }
    /// ```
    fn normalize_desired(
        &self,
        resources: Vec<DesiredResource>,
    ) -> Result<Vec<NormalizedResource>, ProviderError> {
        resources
            .into_iter()
            .map(|resource| resource.finish().map_err(Into::into))
            .collect()
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
        saved_attrs: &HashMap<String, BTreeMap<String, PersistedValue>>,
    ) -> Result<(), ProviderError> {
        let _ = (states, saved_attrs);
        Ok(())
    }
}

/// Fixed guest-transport seam for an initial create. This is a free function,
/// not an overridable provider method, so only SDK glue can mint the attempt.
pub fn dispatch_create<P: CarinaProvider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
) -> Result<ProtocolCreateStep<P::CreateToken>, Box<ProtocolCreateVerdict<P::CreateToken>>> {
    provider
        .create(id, request, CreateAttempt::new())
        .map(CreateStep::into_protocol)
        .map_err(|verdict| Box::new(verdict.into_protocol()))
}

/// Whether a token would move the host's continuation backward in phase.
/// Operation progress may advance to a read-back token, including on the first
/// resume call when persisted state has no phase metadata.
#[doc(hidden)]
pub const fn continuation_phase_regresses(
    host_phase: PendingPhase,
    token_phase: PendingPhase,
) -> bool {
    matches!(
        (host_phase, token_phase),
        (PendingPhase::ReadBack, PendingPhase::Operation)
    )
}

/// Fixed guest-transport seam for polling a host-owned continuation.
///
/// A backward phase transition preserves the previous continuation via `Undetermined`.
/// An identifier-bearing operation continuation may advance to a read-back token.
/// A read-back token without an identifier ends polling via `Untracked`.
pub fn dispatch_poll_create<P: CarinaProvider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
    progress: CreateProgress,
    sent: Sent,
    token: P::CreateToken,
    poll_request: PollRequest,
) -> Result<ProtocolCreateStep<P::CreateToken>, Box<ProtocolPollVerdict<P::CreateToken>>> {
    if continuation_phase_regresses(progress.phase(), token.phase()) {
        return Err(Box::new(types::PollVerdict::Undetermined(
            create_phase_mismatch(id),
        )));
    }
    let poll = match progress.into_identifier() {
        Some(identifier) => Poll::Issued(IssuedCreate::from_parts(identifier, sent), token),
        None => Poll::Unissued(
            PollAttempt::from_sent(sent),
            token.into_operation().map_err(|_token| {
                Box::new(types::PollVerdict::Untracked(read_back_without_identifier(
                    id,
                )))
            })?,
        ),
    };
    provider
        .poll_create(id, request, poll, poll_request)
        .map(CreateStep::into_protocol)
        .map_err(|verdict| Box::new(verdict.into_protocol()))
}

/// Fixed guest-transport seam for resuming a state-owned continuation.
///
/// Applies the same phase and identifier checks as [`dispatch_poll_create`].
pub fn dispatch_resume_create<P: CarinaProvider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
    progress: CreateProgress,
    token: P::CreateToken,
    poll_request: PollRequest,
) -> Result<ProtocolResumeStep<P::CreateToken>, Box<ProtocolResumeVerdict<P::CreateToken>>> {
    if continuation_phase_regresses(progress.phase(), token.phase()) {
        return Err(Box::new(types::ResumeVerdict::Undetermined(
            create_phase_mismatch(id),
        )));
    }
    let resume = match progress.into_identifier() {
        Some(identifier) => Resume::Issued(ResumedCreate::new(identifier), token),
        None => Resume::Unissued(
            ResumeAttempt::new(),
            token.into_operation().map_err(|_token| {
                Box::new(types::ResumeVerdict::Untracked(
                    read_back_without_identifier(id),
                ))
            })?,
        ),
    };
    provider
        .resume_create(id, request, resume, poll_request)
        .map(ResumeStep::into_protocol)
        .map_err(|verdict| Box::new(verdict.into_protocol()))
}

fn create_phase_mismatch(id: &ResourceId) -> ProviderError {
    ProviderError {
        kind: types::ProviderErrorKind::Internal,
        message: "create continuation phase does not match its token".to_string(),
        resource_id: Some(id.clone()),
        cause: None,
        provider_name: None,
        operation: None,
        status: None,
        code: Some("CREATE_CONTINUATION_PHASE_MISMATCH".to_string()),
        request_id: None,
    }
}

fn read_back_without_identifier(id: &ResourceId) -> ProviderError {
    ProviderError {
        kind: types::ProviderErrorKind::InvalidInput,
        message: "read-back continuation requires an identifier".to_string(),
        resource_id: Some(id.clone()),
        cause: None,
        provider_name: None,
        operation: None,
        status: None,
        code: Some("READ_BACK_WITHOUT_IDENTIFIER".to_string()),
        request_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct TestProvider {
        resources: Vec<ValidatedResourceSchema>,
        forms: carina_core::schema::ReferenceForms,
        reference_form_calls: Cell<usize>,
    }

    impl CarinaProvider for TestProvider {
        type CreateToken = TestToken;
        type UpdateToken = Never;
        type DeleteToken = Never;

        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "test".to_string(),
                display_name: "Test".to_string(),
                capabilities: Vec::new(),
                version: "test".to_string(),
            }
        }

        fn schemas(&self) -> Vec<ValidatedResourceSchema> {
            self.resources.clone()
        }

        fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
            ValidatedConfigSchema::default()
        }

        fn reference_forms(&self) -> carina_core::schema::ReferenceForms {
            self.reference_form_calls
                .set(self.reference_form_calls.get() + 1);
            self.forms.clone()
        }

        fn validate_config(&self, _attrs: &BTreeMap<String, Value>) -> Result<(), String> {
            Ok(())
        }

        fn initialize(
            &mut self,
            _attrs: &BTreeMap<String, Value>,
        ) -> Result<ReferenceScope, String> {
            Ok(ReferenceScope::Unavailable)
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> Result<ReadOutcome, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn read_data_source(&self, _resource: &Resource) -> Result<State, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _attempt: CreateAttempt,
        ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>> {
            unimplemented!("schema-only provider")
        }

        fn poll_create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            poll: Poll<Self::CreateToken>,
            _poll_request: PollRequest,
        ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
            Err(match poll {
                Poll::Unissued(attempt, operation) => {
                    attempt.preserve(operation, 7, continuation_error())
                }
                Poll::Issued(issued, token) => {
                    issued.preserve(token, 7, continuation_error()).into()
                }
            })
        }

        fn resume_create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            resume: Resume<Self::CreateToken>,
            _poll_request: PollRequest,
        ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
            Err(match resume {
                Resume::Unissued(attempt, operation) => {
                    attempt.preserve(operation, 9, continuation_error())
                }
                Resume::Issued(resumed, token) => resumed.preserve(token, 9, continuation_error()),
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn poll_update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            _pending: Pending<Self::UpdateToken>,
            _poll_request: PollRequest,
        ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
        ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn poll_delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
            _pending: PendingDelete<Self::DeleteToken>,
        ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
            unimplemented!("schema-only provider")
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> Result<Vec<String>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn reference_identity() -> carina_core::schema::TypeIdentity {
        carina_core::schema::TypeIdentity::new(Some("test"), ["widget"], "Name")
    }

    fn reference_form() -> carina_core::schema::ReferenceForm {
        carina_core::schema::ReferenceForm::Template {
            long: carina_core::schema::ReferenceTemplate::new(
                "arn:{partition}:service:{region}:{account}:widget/{name}",
            )
            .unwrap(),
        }
    }

    #[test]
    fn provider_schema_cache_rejects_a_form_for_an_undeclared_identity() {
        let identity = reference_identity();
        let provider = TestProvider {
            resources: Vec::new(),
            forms: BTreeMap::from([(identity.clone(), reference_form())]),
            reference_form_calls: Cell::new(0),
        };

        let error = match ProviderSchemaCache::from_provider(&provider) {
            Ok(_) => panic!("an undeclared reference form must not be emitted"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            carina_core::schema::ProviderSchemaValidationError::DanglingReferenceForm {
                identity: actual,
            } if actual == identity
        ));
    }

    #[test]
    fn provider_schema_cache_captures_and_projects_reference_forms_once() {
        let identity = reference_identity();
        let schema = carina_core::schema::ResourceSchema::new("test.Widget")
            .attribute(
                carina_core::schema::AttributeSchema::optional(
                    "name",
                    carina_core::schema::AttributeType::refined_string(
                        Some(identity.clone()),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap();
        let provider = TestProvider {
            resources: vec![schema],
            forms: BTreeMap::from([(identity, reference_form())]),
            reference_form_calls: Cell::new(0),
        };

        let cache = ProviderSchemaCache::from_provider(&provider).unwrap();
        assert_eq!(provider.reference_form_calls.get(), 1);
        for _ in 0..2 {
            let json = provider_schema_to_json(cache.witness()).unwrap();
            let envelope: protocol::types::ProviderSchemaEnvelope =
                serde_json::from_str(&json).unwrap();
            assert_eq!(envelope.reference_forms.len(), 1);
        }
        assert_eq!(provider.reference_form_calls.get(), 1);
    }

    #[test]
    fn provider_schema_json_preserves_resource_and_struct_field_groups() {
        use carina_core::schema::{
            AttributeSchema, AttributeType, FieldGroup, ReferenceForms, ResourceSchema,
            StructField, ValidatedConfigSchema, ValidatedProviderSchemaWitness,
        };

        let schema = ResourceSchema::new("test.Widget")
            .attribute(AttributeSchema::optional("first", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("second", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "nested",
                    AttributeType::struct_with_field_groups(
                        "Nested",
                        vec![
                            StructField::optional("left", AttributeType::string()).build(),
                            StructField::optional("right", AttributeType::string()).build(),
                        ],
                        vec![FieldGroup::at_least_one(["left", "right"]).unwrap()],
                    ),
                )
                .build(),
            )
            .field_group(FieldGroup::exactly_one(["first", "second"]).unwrap())
            .validate()
            .unwrap();
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema],
            ValidatedConfigSchema::default(),
            ReferenceForms::new(),
        )
        .unwrap();

        let json = provider_schema_to_json(&witness).unwrap();
        let wire: protocol::types::ProviderSchemaEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(
            wire.resources[0].field_groups[0].kind,
            protocol::types::FieldGroupKind::ExactlyOne
        );
        let protocol::types::AttributeType::Struct { field_groups, .. } =
            &wire.resources[0].attributes["nested"].attr_type
        else {
            panic!("nested attribute must remain a struct");
        };
        assert_eq!(
            field_groups[0].kind,
            protocol::types::FieldGroupKind::AtLeastOne
        );
    }

    #[test]
    fn schema_admission_rejects_a_closed_enum_transform_before_guest_serialization() {
        let error = carina_core::schema::ResourceSchema::new("test.Widget")
            .attribute(
                carina_core::schema::AttributeSchema::optional(
                    "mode",
                    carina_core::schema::AttributeType::enum_(
                        carina_core::schema::TypeIdentity::new(Some("test"), ["widget"], "Mode"),
                        Some(vec!["api-value".to_string()]),
                        Vec::new(),
                        None,
                        Some(carina_core::schema::DslTransform::HyphenToUnderscore),
                    ),
                )
                .build(),
            )
            .validate()
            .expect_err("a validated schema must always be projectable to protocol v18");

        assert!(error.to_string().contains("protocol v18"), "{error}");
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    enum TestToken {
        Operation,
        ReadBack,
    }

    impl PhasedToken for TestToken {
        type Operation = ();

        fn from_operation((): ()) -> Self {
            Self::Operation
        }

        fn into_operation(self) -> Result<(), Self> {
            match self {
                Self::Operation => Ok(()),
                Self::ReadBack => Err(self),
            }
        }

        fn operation(&self) -> Option<&()> {
            match self {
                Self::Operation => Some(&()),
                Self::ReadBack => None,
            }
        }
    }

    fn crud_resource(attributes: BTreeMap<String, Value>) -> types::Resource {
        types::Resource {
            id: types::ResourceId {
                provider: "test".into(),
                resource_type: "test.Widget".into(),
                identity: "example".into(),
            },
            attributes,
            directives: types::Directives::default(),
        }
    }

    fn continuation_error() -> ProviderError {
        ProviderError {
            kind: types::ProviderErrorKind::Internal,
            message: "transient continuation error".to_string(),
            resource_id: None,
            cause: None,
            provider_name: None,
            operation: None,
            status: None,
            code: None,
            request_id: None,
        }
    }

    #[test]
    fn issued_create_preserve_follows_read_back_token_phase() {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let error = continuation_error();
        let verdict = CreateAttempt::new()
            .issued(identifier.clone(), Sent::only(["name"]))
            .preserve(TestToken::ReadBack, 7, error.clone())
            .into_protocol();
        let types::CreateVerdict::InFlight(in_flight) = verdict else {
            panic!("continuation must remain in flight");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::ReadingBack { identifier }
        );
        assert_eq!(in_flight.pending.token, TestToken::ReadBack);
        assert_eq!(in_flight.pending.sent, Sent::only(["name"]));
        assert_eq!(in_flight.pending.poll_after_secs, 7);
        assert_eq!(
            serde_json::to_value(in_flight.error).unwrap(),
            serde_json::to_value(error).unwrap()
        );
    }

    #[test]
    fn issued_create_preserve_follows_operation_token_phase() {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let verdict = CreateAttempt::new()
            .issued(identifier.clone(), Sent::AllRequestAttributes)
            .preserve(TestToken::Operation, 3, continuation_error())
            .into_protocol();
        let types::CreateVerdict::InFlight(in_flight) = verdict else {
            panic!("continuation must remain in flight");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::Operating {
                identifier: Some(identifier)
            }
        );
        assert_eq!(in_flight.pending.token, TestToken::Operation);
        assert_eq!(in_flight.pending.sent, Sent::AllRequestAttributes);
        assert_eq!(in_flight.pending.poll_after_secs, 3);
    }

    #[test]
    fn resumed_create_preserve_follows_read_back_token_phase() {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let error = continuation_error();
        let verdict = ResumeAttempt::new()
            .issued(identifier.clone())
            .preserve(TestToken::ReadBack, 9, error.clone())
            .into_protocol();
        let types::ResumeVerdict::InFlight(in_flight) = verdict else {
            panic!("continuation must remain in flight");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::ReadingBack { identifier }
        );
        assert_eq!(in_flight.pending.token, TestToken::ReadBack);
        assert_eq!(in_flight.pending.poll_after_secs, 9);
        assert_eq!(
            serde_json::to_value(in_flight.error).unwrap(),
            serde_json::to_value(error).unwrap()
        );
    }

    #[test]
    fn resumed_create_preserve_follows_operation_token_phase() {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let verdict = ResumeAttempt::new()
            .issued(identifier.clone())
            .preserve(TestToken::Operation, 5, continuation_error())
            .into_protocol();
        let types::ResumeVerdict::InFlight(in_flight) = verdict else {
            panic!("continuation must remain in flight");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::Operating {
                identifier: Some(identifier)
            }
        );
        assert_eq!(in_flight.pending.token, TestToken::Operation);
        assert_eq!(in_flight.pending.poll_after_secs, 5);
    }

    #[test]
    fn pending_create_and_resume_follow_read_back_token_phase() {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let create = CreateAttempt::new()
            .issued(identifier.clone(), Sent::only(["name"]))
            .pending(TestToken::ReadBack, 7)
            .into_protocol();
        let carina_provider_protocol::types::CreateStep::Pending(create) = create else {
            panic!("issued create must remain pending")
        };
        assert_eq!(
            create.progress,
            CreateProgress::ReadingBack {
                identifier: identifier.clone()
            }
        );
        assert_eq!(create.sent, Sent::only(["name"]));
        let resume = ResumeAttempt::new()
            .issued(identifier.clone())
            .pending(TestToken::ReadBack, 9)
            .into_protocol();
        let carina_provider_protocol::types::ResumeStep::Pending(resume) = resume else {
            panic!("issued resume must remain pending")
        };
        assert_eq!(resume.progress, CreateProgress::ReadingBack { identifier });
    }

    #[test]
    fn pending_update_follows_token_phase() {
        for (token, phase) in [
            (TestToken::Operation, PendingPhase::Operation),
            (TestToken::ReadBack, PendingPhase::ReadBack),
        ] {
            let pending = Pending::new(token, 4);
            assert_eq!(pending.phase(), phase);
            assert_eq!(pending.token, token);
            assert_eq!(pending.poll_after_secs, 4);
        }
    }

    #[test]
    fn operation_payload_round_trips_without_accepting_read_back() {
        assert_eq!(
            TestToken::ReadBack.into_operation().unwrap_err(),
            TestToken::ReadBack
        );
        TestToken::Operation.into_operation().unwrap();
        assert_eq!(TestToken::from_operation(()), TestToken::Operation);
    }

    #[test]
    fn identifierless_dispatch_ends_invalid_read_back_before_calling_provider() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let request = CreateRequest { resource };
        let poll_request = PollRequest {
            finality: PollFinality::Continue,
        };
        let verdict = dispatch_poll_create(
            &provider,
            &id,
            request.clone(),
            CreateProgress::Operating { identifier: None },
            Sent::AllRequestAttributes,
            TestToken::ReadBack,
            poll_request,
        )
        .unwrap_err();
        let types::PollVerdict::Untracked(error) = *verdict else {
            panic!("invalid read-back must end polling, got {verdict:?}");
        };
        assert!(error.message.contains("requires an identifier"));
        assert_eq!(error.code.as_deref(), Some("READ_BACK_WITHOUT_IDENTIFIER"));
        assert_eq!(error.kind, types::ProviderErrorKind::InvalidInput);
        assert_eq!(error.resource_id, Some(id.clone()));

        let verdict = dispatch_resume_create(
            &provider,
            &id,
            request,
            CreateProgress::Operating { identifier: None },
            TestToken::ReadBack,
            poll_request,
        )
        .unwrap_err();
        let types::ResumeVerdict::Untracked(error) = *verdict else {
            panic!("invalid resumed read-back must end polling, got {verdict:?}");
        };
        assert!(error.message.contains("requires an identifier"));
        assert_eq!(error.code.as_deref(), Some("READ_BACK_WITHOUT_IDENTIFIER"));
        assert_eq!(error.kind, types::ProviderErrorKind::InvalidInput);
        assert_eq!(error.resource_id, Some(id));
    }

    #[test]
    fn poll_dispatch_rejects_backward_phase_transition() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let verdict = dispatch_poll_create(
            &provider,
            &id,
            CreateRequest {
                resource: resource.clone(),
            },
            CreateProgress::ReadingBack {
                identifier: AppliedIdentifier::new("created-id").unwrap(),
            },
            Sent::AllRequestAttributes,
            TestToken::Operation,
            PollRequest {
                finality: PollFinality::Continue,
            },
        )
        .unwrap_err();
        let types::PollVerdict::Undetermined(error) = *verdict else {
            panic!("a regression must keep the previous continuation, got {verdict:?}");
        };
        assert!(error.message.contains("phase does not match its token"));
        assert_eq!(
            error.code.as_deref(),
            Some("CREATE_CONTINUATION_PHASE_MISMATCH")
        );
        assert_eq!(error.resource_id, Some(id.clone()));
    }

    #[test]
    fn resume_dispatch_rejects_backward_phase_transition() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let verdict = dispatch_resume_create(
            &provider,
            &id,
            CreateRequest {
                resource: resource.clone(),
            },
            CreateProgress::ReadingBack {
                identifier: AppliedIdentifier::new("created-id").unwrap(),
            },
            TestToken::Operation,
            PollRequest {
                finality: PollFinality::Continue,
            },
        )
        .unwrap_err();
        let types::ResumeVerdict::Undetermined(error) = *verdict else {
            panic!("a regression must keep the previous continuation, got {verdict:?}");
        };
        assert!(error.message.contains("phase does not match its token"));
        assert_eq!(
            error.code.as_deref(),
            Some("CREATE_CONTINUATION_PHASE_MISMATCH")
        );
        assert_eq!(error.resource_id, Some(id.clone()));
    }

    #[test]
    fn poll_dispatch_accepts_forward_read_back_transition() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let sent = Sent::only(["name"]);
        let verdict = dispatch_poll_create(
            &provider,
            &id,
            CreateRequest { resource },
            CreateProgress::Operating {
                identifier: Some(identifier.clone()),
            },
            sent.clone(),
            TestToken::ReadBack,
            PollRequest {
                finality: PollFinality::Continue,
            },
        )
        .unwrap_err();
        let types::PollVerdict::InFlight(in_flight) = *verdict else {
            panic!("a forward transition must reach the provider, got {verdict:?}");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::ReadingBack { identifier }
        );
        assert_eq!(in_flight.pending.token, TestToken::ReadBack);
        assert_eq!(in_flight.pending.sent, sent);
        assert_eq!(in_flight.pending.poll_after_secs, 7);
        assert_eq!(in_flight.error.message, continuation_error().message);
    }

    #[test]
    fn resume_dispatch_accepts_forward_read_back_transition() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        let verdict = dispatch_resume_create(
            &provider,
            &id,
            CreateRequest { resource },
            CreateProgress::Operating {
                identifier: Some(identifier.clone()),
            },
            TestToken::ReadBack,
            PollRequest {
                finality: PollFinality::Continue,
            },
        )
        .unwrap_err();
        let types::ResumeVerdict::InFlight(in_flight) = *verdict else {
            panic!("a forward transition must reach the provider, got {verdict:?}");
        };
        assert_eq!(
            in_flight.pending.progress,
            CreateProgress::ReadingBack { identifier }
        );
        assert_eq!(in_flight.pending.token, TestToken::ReadBack);
        assert_eq!(in_flight.pending.poll_after_secs, 9);
        assert_eq!(in_flight.error.message, continuation_error().message);
    }

    #[test]
    fn update_dispatch_rejects_backward_phase_transition() {
        let token = serde_json::to_string(&TestToken::Operation).unwrap();
        let error =
            wasm_guest::decode_pending_update::<TestToken>(&token, 4, PendingPhase::ReadBack)
                .unwrap_err();
        assert!(error.to_string().contains("phase does not match its token"));
    }

    #[test]
    fn update_dispatch_accepts_forward_read_back_transition() {
        let token = serde_json::to_string(&TestToken::ReadBack).unwrap();
        let pending =
            wasm_guest::decode_pending_update::<TestToken>(&token, 4, PendingPhase::Operation)
                .expect("a read-back token may advance operation progress");
        assert_eq!(pending.token, TestToken::ReadBack);
        assert_eq!(pending.phase(), PendingPhase::ReadBack);
        assert_eq!(pending.poll_after_secs, 4);
    }

    #[test]
    fn update_dispatch_accepts_matching_phases() {
        for token in [TestToken::Operation, TestToken::ReadBack] {
            let encoded = serde_json::to_string(&token).unwrap();
            let pending =
                wasm_guest::decode_pending_update::<TestToken>(&encoded, 4, token.phase()).unwrap();
            assert_eq!(pending.token, token);
            assert_eq!(pending.phase(), token.phase());
            assert_eq!(pending.poll_after_secs, 4);
        }
    }

    fn matching_create_continuations() -> [(CreateProgress, TestToken); 3] {
        let identifier = AppliedIdentifier::new("created-id").unwrap();
        [
            (
                CreateProgress::Operating { identifier: None },
                TestToken::Operation,
            ),
            (
                CreateProgress::Operating {
                    identifier: Some(identifier.clone()),
                },
                TestToken::Operation,
            ),
            (
                CreateProgress::ReadingBack { identifier },
                TestToken::ReadBack,
            ),
        ]
    }

    #[test]
    fn poll_dispatch_preserves_matching_continuations_in_both_phases() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        let sent = Sent::only(["name"]);
        for (progress, token) in matching_create_continuations() {
            let verdict = dispatch_poll_create(
                &provider,
                &id,
                CreateRequest {
                    resource: resource.clone(),
                },
                progress.clone(),
                sent.clone(),
                token,
                PollRequest {
                    finality: PollFinality::Continue,
                },
            )
            .unwrap_err();
            let types::PollVerdict::InFlight(in_flight) = *verdict else {
                panic!("a matching continuation must reach the provider, got {verdict:?}");
            };
            assert_eq!(in_flight.pending.progress, progress);
            assert_eq!(in_flight.pending.token, token);
            assert_eq!(in_flight.pending.sent, sent);
            assert_eq!(in_flight.pending.poll_after_secs, 7);
            assert_eq!(in_flight.error.message, continuation_error().message);
        }
    }

    #[test]
    fn resume_dispatch_preserves_matching_continuations_in_both_phases() {
        let provider = TestProvider {
            resources: Vec::new(),
            forms: ReferenceForms::new(),
            reference_form_calls: Cell::new(0),
        };
        let resource = crud_resource(BTreeMap::new());
        let id = resource.id.clone();
        for (progress, token) in matching_create_continuations() {
            let verdict = dispatch_resume_create(
                &provider,
                &id,
                CreateRequest {
                    resource: resource.clone(),
                },
                progress.clone(),
                token,
                PollRequest {
                    finality: PollFinality::Continue,
                },
            )
            .unwrap_err();
            let types::ResumeVerdict::InFlight(in_flight) = *verdict else {
                panic!("a matching continuation must reach the provider, got {verdict:?}");
            };
            assert_eq!(in_flight.pending.progress, progress);
            assert_eq!(in_flight.pending.token, token);
            assert_eq!(in_flight.pending.poll_after_secs, 9);
            assert_eq!(in_flight.error.message, continuation_error().message);
        }
    }

    #[test]
    fn never_token_decode_always_fails() {
        for encoded in ["null", "{}", "0", "\"token\""] {
            assert!(serde_json::from_str::<Never>(encoded).is_err());
        }
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
