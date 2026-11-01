//! Carina provider SDK for building WebAssembly component providers.
//!
//! Implement [`CarinaProvider`] and export it with [`export_provider!`].

pub use carina_core as core;
pub use carina_provider_protocol as protocol;
pub use carina_provider_protocol::types;

/// The "Schemas" page of the provider guide on carina-rs.dev. Its Rust
/// examples run as doctests of this crate so they cannot drift from the SDK.
#[cfg(doctest)]
#[doc = include_str!("../../site/src/content/providers/writing/schemas.md")]
pub struct ProviderGuideSchemasPage;

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

/// Failure while projecting validated SDK schemas onto version-13 JSON.
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
/// The validated provider schemas determine the scalar kind. The version-13
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Pending<T> {
    pub token: T,
    pub poll_after_secs: u32,
    pub phase: carina_provider_protocol::PendingPhase,
}

pub use carina_provider_protocol::types::{PendingDelete, PendingToken, Sent};
pub use carina_provider_protocol::{PendingPhase, PollFinality, PollRequest};

impl<T> Pending<T> {
    pub fn operation(token: T, poll_after_secs: u32) -> Self {
        Self {
            token,
            poll_after_secs,
            phase: PendingPhase::Operation,
        }
    }

    pub fn read_back(token: T, poll_after_secs: u32) -> Self {
        Self {
            token,
            poll_after_secs,
            phase: PendingPhase::ReadBack,
        }
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

    pub fn pending<T>(self, token: T, sent: Sent, poll_after_secs: u32) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            carina_provider_protocol::types::PendingCreate {
                token,
                identifier: None,
                sent,
                poll_after_secs,
                phase: PendingPhase::Operation,
            },
        ))
    }

    pub fn preserve<T>(
        self,
        token: T,
        sent: Sent,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: carina_provider_protocol::types::PendingCreate {
                    token,
                    identifier: None,
                    sent,
                    poll_after_secs,
                    phase: PendingPhase::Operation,
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

    pub fn pending<T>(self, token: T, poll_after_secs: u32) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            carina_provider_protocol::types::PendingCreate {
                token,
                identifier: None,
                sent: self.sent,
                poll_after_secs,
                phase: PendingPhase::Operation,
            },
        ))
    }

    pub fn preserve<T>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> PollVerdict<T> {
        PollVerdict(carina_provider_protocol::types::PollVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: carina_provider_protocol::types::PendingCreate {
                    token,
                    identifier: None,
                    sent: self.sent,
                    poll_after_secs,
                    phase: PendingPhase::Operation,
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

    pub fn pending<T>(self, token: T, poll_after_secs: u32) -> CreateStep<T> {
        self.pending_in_phase(token, poll_after_secs, PendingPhase::Operation)
    }

    /// Continue polling the post-create read after the identifier is known.
    pub fn pending_read_back<T>(self, token: T, poll_after_secs: u32) -> CreateStep<T> {
        self.pending_in_phase(token, poll_after_secs, PendingPhase::ReadBack)
    }

    fn pending_in_phase<T>(
        self,
        token: T,
        poll_after_secs: u32,
        phase: PendingPhase,
    ) -> CreateStep<T> {
        CreateStep(carina_provider_protocol::types::CreateStep::Pending(
            carina_provider_protocol::types::PendingCreate {
                token,
                identifier: Some(self.identifier),
                sent: self.sent,
                poll_after_secs,
                phase,
            },
        ))
    }

    pub fn preserve<T>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> CreateVerdict<T> {
        CreateVerdict(carina_provider_protocol::types::CreateVerdict::InFlight(
            carina_provider_protocol::types::CreateInFlight {
                pending: carina_provider_protocol::types::PendingCreate {
                    token,
                    identifier: Some(self.identifier),
                    sent: self.sent,
                    poll_after_secs,
                    phase: PendingPhase::Operation,
                },
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

    pub fn pending<T>(self, token: T, poll_after_secs: u32) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Pending(
            carina_provider_protocol::types::ResumePending {
                token,
                identifier: None,
                poll_after_secs,
                phase: PendingPhase::Operation,
            },
        ))
    }

    pub fn preserve<T>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::InFlight(
            carina_provider_protocol::types::ResumeInFlight {
                pending: carina_provider_protocol::types::ResumePending {
                    token,
                    identifier: None,
                    poll_after_secs,
                    phase: PendingPhase::Operation,
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

    pub fn pending<T>(self, token: T, poll_after_secs: u32) -> ResumeStep<T> {
        self.pending_in_phase(token, poll_after_secs, PendingPhase::Operation)
    }

    pub fn pending_read_back<T>(self, token: T, poll_after_secs: u32) -> ResumeStep<T> {
        self.pending_in_phase(token, poll_after_secs, PendingPhase::ReadBack)
    }

    fn pending_in_phase<T>(
        self,
        token: T,
        poll_after_secs: u32,
        phase: PendingPhase,
    ) -> ResumeStep<T> {
        ResumeStep(carina_provider_protocol::types::ResumeStep::Pending(
            carina_provider_protocol::types::ResumePending {
                token,
                identifier: Some(self.identifier),
                poll_after_secs,
                phase,
            },
        ))
    }

    pub fn preserve<T>(
        self,
        token: T,
        poll_after_secs: u32,
        error: ProviderError,
    ) -> ResumeVerdict<T> {
        ResumeVerdict(carina_provider_protocol::types::ResumeVerdict::InFlight(
            carina_provider_protocol::types::ResumeInFlight {
                pending: carina_provider_protocol::types::ResumePending {
                    token,
                    identifier: Some(self.identifier),
                    poll_after_secs,
                    phase: PendingPhase::Operation,
                },
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
pub enum Poll<T> {
    Unissued(PollAttempt, T),
    Issued(IssuedCreate, T),
}

/// Typestate and provider-owned token passed to a persisted create resume.
#[derive(Debug)]
pub enum Resume<T> {
    Unissued(ResumeAttempt, T),
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

/// The state of one top-level desired resource attribute at normalization time.
///
/// The host holds unresolved attributes back from the WASM value boundary. A
/// provider must therefore distinguish an attribute the user omitted from one
/// whose value exists but cannot be observed until its dependency resolves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DesiredAttribute<T> {
    Known(T),
    Absent,
    Deferred,
}

/// A desired resource decoded from the WASM normalization boundary.
///
/// Attribute storage and the host's deferred-name set stay private so callers
/// cannot accidentally interpret a missing map entry as user-authored
/// absence. Use [`DesiredResource::attribute`] or
/// [`DesiredResource::attribute_mut`] for one attribute, or
/// [`DesiredResource::attributes`] to walk every known and deferred attribute.
#[derive(Debug)]
pub struct DesiredResource {
    resource: Resource,
    deferred: std::collections::BTreeSet<String>,
}

/// A desired resource whose normalization is complete.
///
/// This type is intentionally opaque. Provider implementations can create it
/// only by calling [`DesiredResource::finish`]; the SDK's guest glue is solely
/// responsible for encoding it onto the WASM boundary.
///
/// ```compile_fail
/// use carina_plugin_sdk::{NormalizedResource, types::Resource};
///
/// fn expose_raw(normalized: NormalizedResource) -> Resource {
///     let NormalizedResource { resource } = normalized;
///     resource
/// }
/// ```
#[derive(Debug)]
pub struct NormalizedResource {
    // Guest encoding exists only for WASM builds; host-only SDK builds still
    // need this opaque return type in the public trait signature.
    #[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
    resource: Resource,
}

/// An attempted insertion targeted an attribute that was not absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredAttributeError {
    /// The name already has a known value.
    Known { name: String },
    /// The host is withholding the name's unresolved value.
    Deferred { name: String },
}

impl std::fmt::Display for DesiredAttributeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Known { name } => write!(
                formatter,
                "attribute '{name}' is known; insert_absent requires an absent attribute"
            ),
            Self::Deferred { name } => write!(
                formatter,
                "attribute '{name}' is deferred; insert_absent requires an absent attribute"
            ),
        }
    }
}

impl std::error::Error for DesiredAttributeError {}

/// Invalid `desired-resource` input received from the WASM host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesiredResourceDecodeError {
    DuplicateDeferredAttribute { name: String },
    DeferredAttributePresent { name: String },
}

impl std::fmt::Display for DesiredResourceDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateDeferredAttribute { name } => {
                write!(
                    formatter,
                    "deferred attribute name '{name}' appears more than once"
                )
            }
            Self::DeferredAttributePresent { name } => write!(
                formatter,
                "attribute '{name}' is both present in the resource and marked deferred"
            ),
        }
    }
}

impl std::error::Error for DesiredResourceDecodeError {}

impl DesiredResource {
    #[cfg(any(target_arch = "wasm32", test))]
    fn decode(
        resource: Resource,
        deferred: Vec<String>,
    ) -> Result<Self, DesiredResourceDecodeError> {
        let mut deferred_names = std::collections::BTreeSet::new();
        for name in deferred {
            if !deferred_names.insert(name.clone()) {
                return Err(DesiredResourceDecodeError::DuplicateDeferredAttribute { name });
            }
        }
        if let Some(name) = deferred_names
            .iter()
            .find(|name| resource.attributes.contains_key(name.as_str()))
        {
            return Err(DesiredResourceDecodeError::DeferredAttributePresent {
                name: name.clone(),
            });
        }
        Ok(Self {
            resource,
            deferred: deferred_names,
        })
    }

    /// Resource identity used for provider dispatch and diagnostics.
    pub fn id(&self) -> &ResourceId {
        &self.resource.id
    }

    /// Query one top-level attribute without conflating absence and deferral.
    pub fn attribute(&self, name: &str) -> DesiredAttribute<&Value> {
        if self.deferred.contains(name) {
            DesiredAttribute::Deferred
        } else {
            self.resource
                .attributes
                .get(name)
                .map_or(DesiredAttribute::Absent, DesiredAttribute::Known)
        }
    }

    /// Mutably query one known top-level attribute.
    ///
    /// Deferred and absent attributes cannot be synthesized through this
    /// method; both remain explicit enum variants.
    pub fn attribute_mut(&mut self, name: &str) -> DesiredAttribute<&mut Value> {
        if self.deferred.contains(name) {
            DesiredAttribute::Deferred
        } else {
            self.resource
                .attributes
                .get_mut(name)
                .map_or(DesiredAttribute::Absent, DesiredAttribute::Known)
        }
    }

    /// Iterate over every known and deferred top-level attribute.
    ///
    /// Absent attributes have no name to yield. Each deferred name is emitted
    /// explicitly as [`DesiredAttribute::Deferred`].
    pub fn attributes(&self) -> impl Iterator<Item = (&str, DesiredAttribute<&Value>)> {
        let known = self
            .resource
            .attributes
            .iter()
            .map(|(name, value)| (name.as_str(), DesiredAttribute::Known(value)));
        let deferred = self
            .deferred
            .iter()
            .map(|name| (name.as_str(), DesiredAttribute::Deferred));
        known.chain(deferred)
    }

    /// Insert a top-level attribute only when it is truly absent.
    ///
    /// Known and deferred names produce distinct typed errors. In particular,
    /// a deferred name cannot be replaced while its value is withheld by the
    /// host.
    pub fn insert_absent(
        &mut self,
        name: impl Into<String>,
        value: Value,
    ) -> Result<(), DesiredAttributeError> {
        use std::collections::btree_map::Entry;

        let name = name.into();
        if self.deferred.contains(&name) {
            return Err(DesiredAttributeError::Deferred { name });
        }
        match self.resource.attributes.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(value);
                Ok(())
            }
            Entry::Occupied(entry) => Err(DesiredAttributeError::Known {
                name: entry.key().clone(),
            }),
        }
    }

    /// Finish normalization and seal the resource for the SDK's guest glue.
    pub fn finish(self) -> NormalizedResource {
        NormalizedResource {
            resource: self.resource,
        }
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
    /// return a pending step again and must not wait in-call. When
    /// `poll_request.finality` is [`PollFinality::FinalAttempt`] after
    /// read-back has begun, it must not return pending in either phase; finish
    /// with success, partial success, or an error. A read-back continuation
    /// must never regress to operation phase.
    fn poll_create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        poll: Poll<Self::CreateToken>,
        poll_request: PollRequest,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>>;

    /// Continue a create operation loaded from state. `poll_request` has the
    /// same host-owned finality and monotonic read-back semantics as
    /// [`CarinaProvider::poll_create`].
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
    /// in-call. A [`PollFinality::FinalAttempt`] request after read-back has
    /// begun must not return pending in either phase, and read-back must never
    /// regress to operation phase.
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

    /// Normalize desired resources (optional).
    ///
    /// Every attribute lookup is a three-way query: [`DesiredAttribute::Known`],
    /// [`DesiredAttribute::Absent`], or [`DesiredAttribute::Deferred`], because
    /// the host temporarily withholds unresolved values from WASM. Use
    /// [`DesiredResource::attributes`] for a generic walk,
    /// [`DesiredResource::attribute_mut`] to edit known values, and
    /// [`DesiredResource::insert_absent`] to add a value only when it is truly
    /// absent. Return each resource by calling [`DesiredResource::finish`].
    fn normalize_desired(
        &self,
        resources: Vec<DesiredResource>,
    ) -> Result<Vec<NormalizedResource>, ProviderError> {
        Ok(resources.into_iter().map(DesiredResource::finish).collect())
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

/// Fixed guest-transport seam for polling a host-owned continuation.
pub fn dispatch_poll_create<P: CarinaProvider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
    identifier: Option<AppliedIdentifier>,
    sent: Sent,
    token: P::CreateToken,
    poll_request: PollRequest,
) -> Result<ProtocolCreateStep<P::CreateToken>, Box<ProtocolPollVerdict<P::CreateToken>>> {
    let poll = match identifier {
        Some(identifier) => Poll::Issued(IssuedCreate::from_parts(identifier, sent), token),
        None => Poll::Unissued(PollAttempt::from_sent(sent), token),
    };
    provider
        .poll_create(id, request, poll, poll_request)
        .map(CreateStep::into_protocol)
        .map_err(|verdict| Box::new(verdict.into_protocol()))
}

/// Fixed guest-transport seam for resuming a state-owned continuation.
pub fn dispatch_resume_create<P: CarinaProvider + ?Sized>(
    provider: &P,
    id: &ResourceId,
    request: CreateRequest,
    identifier: Option<AppliedIdentifier>,
    token: P::CreateToken,
    poll_request: PollRequest,
) -> Result<ProtocolResumeStep<P::CreateToken>, Box<ProtocolResumeVerdict<P::CreateToken>>> {
    let resume = match identifier {
        Some(identifier) => Resume::Issued(ResumedCreate::new(identifier), token),
        None => Resume::Unissued(ResumeAttempt::new(), token),
    };
    provider
        .resume_create(id, request, resume, poll_request)
        .map(ResumeStep::into_protocol)
        .map_err(|verdict| Box::new(verdict.into_protocol()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct SchemaOnlyProvider {
        resources: Vec<ValidatedResourceSchema>,
        forms: carina_core::schema::ReferenceForms,
        reference_form_calls: Cell<usize>,
    }

    impl CarinaProvider for SchemaOnlyProvider {
        type CreateToken = Never;
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
            _poll: Poll<Self::CreateToken>,
            _poll_request: PollRequest,
        ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
            unimplemented!("schema-only provider")
        }

        fn resume_create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _resume: Resume<Self::CreateToken>,
            _poll_request: PollRequest,
        ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
            unimplemented!("schema-only provider")
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
        let provider = SchemaOnlyProvider {
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
        let provider = SchemaOnlyProvider {
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
            .expect_err("a validated schema must always be projectable to protocol v13");

        assert!(error.to_string().contains("protocol v13"), "{error}");
    }

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

    fn desired_resource(attributes: BTreeMap<String, Value>) -> Resource {
        Resource {
            id: ResourceId {
                provider: "test".to_string(),
                resource_type: "test.Widget".to_string(),
                identity: "example".to_string(),
            },
            attributes,
            directives: types::Directives::default(),
        }
    }

    #[test]
    fn desired_resource_exposes_known_absent_and_deferred_attributes() {
        let mut desired = DesiredResource::decode(
            desired_resource(BTreeMap::from([(
                "known".to_string(),
                Value::String("before".to_string()),
            )])),
            vec!["deferred".to_string()],
        )
        .expect("valid desired resource");

        assert!(matches!(
            desired.attribute("known"),
            DesiredAttribute::Known(Value::String(value)) if value == "before"
        ));
        assert!(matches!(
            desired.attribute("absent"),
            DesiredAttribute::Absent
        ));
        assert!(matches!(
            desired.attribute("deferred"),
            DesiredAttribute::Deferred
        ));

        match desired.attribute_mut("known") {
            DesiredAttribute::Known(value) => *value = Value::String("after".to_string()),
            DesiredAttribute::Absent | DesiredAttribute::Deferred => {
                panic!("known attribute must remain mutable")
            }
        }
        assert!(matches!(
            desired.attribute_mut("deferred"),
            DesiredAttribute::Deferred
        ));

        let normalized = desired.finish();
        assert_eq!(
            normalized.resource.attributes.get("known"),
            Some(&Value::String("after".to_string()))
        );
        assert!(!normalized.resource.attributes.contains_key("deferred"));
    }

    #[test]
    fn desired_resource_iteration_and_absent_insertion_preserve_three_way_state() {
        let mut desired = DesiredResource::decode(
            desired_resource(BTreeMap::from([(
                "known".to_string(),
                Value::String("configured".to_string()),
            )])),
            vec!["deferred".to_string()],
        )
        .expect("valid desired resource");

        let states = desired
            .attributes()
            .map(|(name, attribute)| {
                let state = match attribute {
                    DesiredAttribute::Known(_) => "known",
                    DesiredAttribute::Deferred => "deferred",
                    DesiredAttribute::Absent => unreachable!("absent names are not iterable"),
                };
                (name, state)
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            states,
            BTreeMap::from([("deferred", "deferred"), ("known", "known")])
        );

        let deferred_error = desired
            .insert_absent("deferred", Value::String("replacement".to_string()))
            .expect_err("a deferred attribute is not absent");
        assert_eq!(
            deferred_error,
            DesiredAttributeError::Deferred {
                name: "deferred".to_string()
            }
        );

        let known_error = desired
            .insert_absent("known", Value::String("replacement".to_string()))
            .expect_err("a known attribute is not absent");
        assert_eq!(
            known_error,
            DesiredAttributeError::Known {
                name: "known".to_string()
            }
        );

        desired
            .insert_absent("absent", Value::String("default".to_string()))
            .expect("an absent attribute accepts a default");
        assert!(matches!(
            desired.attribute("absent"),
            DesiredAttribute::Known(Value::String(value)) if value == "default"
        ));
    }

    #[test]
    fn desired_resource_rejects_duplicate_deferred_names() {
        let error = DesiredResource::decode(
            desired_resource(BTreeMap::new()),
            vec!["pending".to_string(), "pending".to_string()],
        )
        .expect_err("duplicate deferred names must be rejected");

        assert!(matches!(
            error,
            DesiredResourceDecodeError::DuplicateDeferredAttribute { name }
                if name == "pending"
        ));
    }

    #[test]
    fn desired_resource_rejects_present_deferred_attribute() {
        let error = DesiredResource::decode(
            desired_resource(BTreeMap::from([(
                "pending".to_string(),
                Value::String("guest-visible".to_string()),
            )])),
            vec!["pending".to_string()],
        )
        .expect_err("an attribute cannot be both present and deferred");

        assert!(matches!(
            error,
            DesiredResourceDecodeError::DeferredAttributePresent { name }
                if name == "pending"
        ));
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
