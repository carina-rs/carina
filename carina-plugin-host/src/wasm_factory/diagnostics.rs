//! Host-owned formatting for typed guest decoding/runtime failures.
use carina_core::provider::EmptyAppliedIdentifier;
use carina_core::provider_text::{DiagnosticText, ProviderCause, ProviderDiagnostic, ProviderText};
use carina_core::resource::ResourceId as CoreResourceId;
use std::collections::BTreeSet;
use std::fmt;
use std::time::Duration;

/// The wire has already flattened this cause; only the host decoder can
/// rehydrate its text before core captures and binds the diagnostic.
#[derive(Debug)]
pub(in crate::wasm_factory) struct FlattenedWireCause(String);

impl FlattenedWireCause {
    pub(in crate::wasm_factory) fn new(text: String) -> Self {
        Self(text)
    }
}

impl fmt::Display for FlattenedWireCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlattenedWireCause {}

/// Error raised when provider-emitted schema wire data cannot be decoded.
///
/// External provider metadata is version-gated but still untrusted at this
/// boundary; carina#3459 makes unsupported wire shapes explicit errors instead
/// of panics so callers can attach provider context.
#[derive(Debug)]
pub enum SchemaDecodeError {
    JsonParse(MetadataDiagnostic),
    Protocol(carina_core::protocol_convert::SchemaDecodeError),
}

impl SchemaDecodeError {
    pub(in crate::wasm_factory) fn schema_json_parse(err: serde_json::Error) -> Self {
        Self::JsonParse(MetadataDiagnostic::new(err))
    }

    pub(in crate::wasm_factory) fn protocol_conversion(
        err: carina_core::protocol_convert::SchemaDecodeError,
    ) -> Self {
        Self::Protocol(err)
    }
}

impl fmt::Display for SchemaDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::JsonParse(_) => f.write_str("schema JSON parse error"),
            Self::Protocol(_) => f.write_str("schema protocol conversion failed"),
        }
    }
}

impl std::error::Error for SchemaDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::JsonParse(source) => Some(source),
            Self::Protocol(source) => Some(source),
        }
    }
}

/// Failure to decode a raw scalar or JSON-backed recursive value at the WIT boundary.
///
/// The payload itself is deliberately not retained: `secret-val` may contain
/// plaintext credentials. JSON syntax failures retain the structured serde
/// error as typed data and render its diagnostic once without exposing the
/// input.
#[derive(Debug)]
pub(in crate::wasm_factory) struct WasmValueDecodeError {
    pub(in crate::wasm_factory) wit_variant: WitValueVariant,
    pub(in crate::wasm_factory) attribute_path: Vec<DiagnosticText>,
    pub(in crate::wasm_factory) kind: WasmValueDecodeErrorKind,
}

#[derive(Debug, Clone, Copy)]
pub(in crate::wasm_factory) enum WitValueVariant {
    Float,
    StringList,
    List,
    Map,
    Secret,
    RedactedSecret,
}

impl fmt::Display for WitValueVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Float => "float-val",
            Self::StringList => "string-list-val",
            Self::List => "list-val",
            Self::Map => "map-val",
            Self::Secret => "secret-val",
            Self::RedactedSecret => "redacted-secret-val",
        })
    }
}

#[derive(Debug)]
pub(in crate::wasm_factory) enum WasmValueDecodeErrorKind {
    InvalidFloat(carina_provider_protocol::NonFiniteFloatError),
    InvalidJson(serde_json::Error),
    ExpectedArray,
    ExpectedObject,
    ExpectedString { index: usize },
    Null,
    UnsupportedNumber,
    MalformedPersistedSecret,
    ReservedPersistedKey,
    InvalidRedactedSecretDigest,
}

impl WasmValueDecodeError {
    pub(in crate::wasm_factory) fn new(
        wit_variant: WitValueVariant,
        kind: WasmValueDecodeErrorKind,
    ) -> Self {
        Self {
            wit_variant,
            attribute_path: Vec::new(),
            kind,
        }
    }

    pub(in crate::wasm_factory) fn prepend_attribute(mut self, name: &str) -> Self {
        self.attribute_path.insert(0, name.into());
        self
    }
}

impl fmt::Display for WasmValueDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to decode WIT ")?;
        self.wit_variant.fmt(f)?;
        if !self.attribute_path.is_empty() {
            f.write_str(" attribute '")?;
            DiagnosticText::new(self.attribute_path.join(".")).fmt(f)?;
            f.write_str("'")?;
        }
        match &self.kind {
            WasmValueDecodeErrorKind::InvalidFloat(_) => Ok(()),
            WasmValueDecodeErrorKind::InvalidJson(error) => {
                f.write_str(": invalid JSON: ")?;
                error.fmt(f)
            }
            WasmValueDecodeErrorKind::ExpectedArray => f.write_str(": expected a JSON array"),
            WasmValueDecodeErrorKind::ExpectedObject => f.write_str(": expected a JSON object"),
            WasmValueDecodeErrorKind::ExpectedString { index } => {
                f.write_str(": expected a JSON string at array index ")?;
                f.write_str(&index.to_string())
            }
            WasmValueDecodeErrorKind::Null => {
                f.write_str(": JSON null is not supported by the value protocol")
            }
            WasmValueDecodeErrorKind::UnsupportedNumber => {
                f.write_str(": JSON number cannot be represented by the value protocol")
            }
            WasmValueDecodeErrorKind::MalformedPersistedSecret => {
                f.write_str(": malformed persisted redacted-secret tag")
            }
            WasmValueDecodeErrorKind::ReservedPersistedKey => {
                f.write_str(": unescaped reserved persisted-state object key")
            }
            WasmValueDecodeErrorKind::InvalidRedactedSecretDigest => {
                f.write_str(": invalid redacted-secret digest")
            }
        }
    }
}

impl std::error::Error for WasmValueDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            WasmValueDecodeErrorKind::InvalidFloat(source) => Some(source),
            WasmValueDecodeErrorKind::InvalidJson(_source) => None,
            WasmValueDecodeErrorKind::ExpectedArray
            | WasmValueDecodeErrorKind::ExpectedObject
            | WasmValueDecodeErrorKind::ExpectedString { index: _ }
            | WasmValueDecodeErrorKind::Null
            | WasmValueDecodeErrorKind::UnsupportedNumber
            | WasmValueDecodeErrorKind::MalformedPersistedSecret
            | WasmValueDecodeErrorKind::ReservedPersistedKey
            | WasmValueDecodeErrorKind::InvalidRedactedSecretDigest => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::wasm_factory) enum AppliedStateOperation {
    Read,
    Create,
    Update,
}

impl fmt::Display for AppliedStateOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Create => "create",
            Self::Update => "update",
        })
    }
}

#[derive(Debug)]
pub(in crate::wasm_factory) enum WasmAppliedStateDecodeErrorKind {
    Attributes(Box<WasmValueDecodeError>),
    Identifier(EmptyAppliedIdentifier),
}

/// Failure to decode an existing-resource observation returned across the WIT
/// boundary.
///
/// The operation and request resource ID are host-owned context. Causes are
/// retained through the typed source chain.
#[derive(Debug)]
pub(in crate::wasm_factory) struct WasmAppliedStateDecodeError {
    pub(in crate::wasm_factory) operation: AppliedStateOperation,
    pub(in crate::wasm_factory) resource_id: Box<CoreResourceId>,
    pub(in crate::wasm_factory) kind: WasmAppliedStateDecodeErrorKind,
}

impl WasmAppliedStateDecodeError {
    pub(in crate::wasm_factory) fn attributes(
        operation: AppliedStateOperation,
        resource_id: &CoreResourceId,
        source: WasmValueDecodeError,
    ) -> Self {
        Self {
            operation,
            resource_id: Box::new(resource_id.clone()),
            kind: WasmAppliedStateDecodeErrorKind::Attributes(Box::new(source)),
        }
    }

    pub(in crate::wasm_factory) fn identifier(
        operation: AppliedStateOperation,
        resource_id: &CoreResourceId,
        source: EmptyAppliedIdentifier,
    ) -> Self {
        Self {
            operation,
            resource_id: Box::new(resource_id.clone()),
            kind: WasmAppliedStateDecodeErrorKind::Identifier(source),
        }
    }
}

impl fmt::Display for WasmAppliedStateDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            WasmAppliedStateDecodeErrorKind::Attributes(_) => {
                f.write_str("failed to decode WIT ")?;
                self.operation.fmt(f)?;
                f.write_str(" outcome applied state attributes for resource '")?;
                self.resource_id.fmt(f)?;
                f.write_str("'")
            }
            WasmAppliedStateDecodeErrorKind::Identifier(error) => {
                f.write_str("failed to decode WIT ")?;
                self.operation.fmt(f)?;
                f.write_str(" outcome applied state identifier for resource '")?;
                self.resource_id.fmt(f)?;
                f.write_str("': ")?;
                error.fmt(f)
            }
        }
    }
}

impl std::error::Error for WasmAppliedStateDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            WasmAppliedStateDecodeErrorKind::Attributes(source) => Some(source.as_ref()),
            WasmAppliedStateDecodeErrorKind::Identifier(_source) => None,
        }
    }
}

#[derive(Clone)]
pub(in crate::wasm_factory) struct PendingToken(pub(in crate::wasm_factory) String);

impl fmt::Debug for PendingToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted pending token>")
    }
}

pub(in crate::wasm_factory) struct MetadataWasmChain {
    pub(in crate::wasm_factory) error: wasmtime::Error,
    pub(in crate::wasm_factory) inline: bool,
}
impl fmt::Debug for MetadataWasmChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WASM metadata error")
    }
}
impl fmt::Display for MetadataWasmChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, cause) in self.error.chain().enumerate() {
            if index > 0 {
                f.write_str(if self.inline { ": " } else { "\n  caused by: " })?;
            }
            f.write_str(&cause.to_string())?;
        }
        Ok(())
    }
}
impl std::error::Error for MetadataWasmChain {}

#[derive(Debug)]
pub(in crate::wasm_factory) struct WasmNormalizerResultKeysError {
    pub(in crate::wasm_factory) missing: BTreeSet<String>,
    pub(in crate::wasm_factory) unexpected: BTreeSet<String>,
    pub(in crate::wasm_factory) duplicates: BTreeSet<String>,
}

impl fmt::Display for WasmNormalizerResultKeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("returned a mismatched state key set")?;
        for (label, keys) in [
            ("missing", &self.missing),
            ("unexpected", &self.unexpected),
            ("duplicate", &self.duplicates),
        ] {
            if !keys.is_empty() {
                let names = keys
                    .iter()
                    .map(|key| if key.is_empty() { "\"\"" } else { key.as_str() })
                    .collect::<Vec<_>>()
                    .join(", ");
                f.write_str("; ")?;
                f.write_str(label)?;
                f.write_str(": ")?;
                DiagnosticText::new(names).fmt(f)?;
            }
        }
        Ok(())
    }
}

impl std::error::Error for WasmNormalizerResultKeysError {}

#[derive(Debug)]
pub(in crate::wasm_factory) struct WasmNormalizerResultCountError {
    pub(in crate::wasm_factory) actual: usize,
}

impl fmt::Display for WasmNormalizerResultCountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("returned ")?;
        f.write_str(&self.actual.to_string())?;
        f.write_str(" state entries for a single-resource call; expected exactly one")
    }
}

impl std::error::Error for WasmNormalizerResultCountError {}

#[derive(Clone, Copy, Debug)]
pub(in crate::wasm_factory) struct WallBudgetExceeded {
    pub(in crate::wasm_factory) wall: Duration,
}

impl fmt::Display for WallBudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "guest call exceeded its {}s wall-clock budget (time spent in host I/O counts toward \
             it; check network connectivity to the provider's endpoints)",
            self.wall.as_secs_f64()
        )
    }
}

impl std::error::Error for WallBudgetExceeded {}

/// An error captured by the metadata loaders before any configured input exists.
/// Runtime callers cannot mint this display authority.
///
/// ```compile_fail,E0624
/// use carina_plugin_host::wasm_factory::MetadataDiagnostic;
/// let _ = MetadataDiagnostic::new(std::io::Error::other("guest private"));
/// ```
#[derive(Clone, Debug)]
pub struct MetadataDiagnostic {
    source: ProviderDiagnostic,
    projection: DiagnosticText,
    opaque: ProviderText,
}
impl MetadataDiagnostic {
    pub(in crate::wasm_factory) fn new(
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let mut text = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            text.push_str(": ");
            text.push_str(&cause.to_string());
            source = cause.source();
        }
        let source = ProviderDiagnostic::new(error);
        Self {
            source,
            opaque: ProviderText::new(text.clone()),
            projection: DiagnosticText::new(text),
        }
    }
    pub fn opaque(&self) -> ProviderText {
        self.opaque.clone()
    }
    pub fn is<T: std::error::Error + 'static>(&self) -> bool {
        self.source.is::<T>()
    }
    pub fn as_cause(&self) -> ProviderCause<'_> {
        self.source.as_cause()
    }
    #[cfg(test)]
    pub(in crate::wasm_factory) fn error(&self) -> ProviderCause<'_> {
        self.as_cause()
    }
}
impl fmt::Display for MetadataDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.projection.fmt(f)
    }
}
impl std::error::Error for MetadataDiagnostic {}

/// One immutable metadata failure, with a bounded standalone display and the
/// complete typed diagnostic used when the load error becomes a cause.
#[derive(Clone, Debug)]
pub struct MetadataLoadDiagnostic {
    context: DiagnosticText,
    source: MetadataDiagnostic,
    captured: carina_core::provider_text::RenderedGuestDiagnostic,
}

impl MetadataLoadDiagnostic {
    pub(in crate::wasm_factory) fn new(
        context: impl Into<String>,
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let context = context.into();
        let source = MetadataDiagnostic::new(error);
        let captured =
            ProviderDiagnostic::new(carina_core::provider::ProviderError::internal_caused_by(
                ProviderText::new(context.clone()),
                source.source.clone(),
            ));
        let captured = captured.into();
        Self {
            context: DiagnosticText::new(context),
            source,
            captured,
        }
    }

    pub(in crate::wasm_factory) fn captured(&self) -> &(dyn std::error::Error + 'static) {
        &self.captured
    }

    pub fn is<T: std::error::Error + 'static>(&self) -> bool {
        self.source.is::<T>()
    }

    pub fn as_cause(&self) -> ProviderCause<'_> {
        self.source.as_cause()
    }
}

impl fmt::Display for MetadataLoadDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}
