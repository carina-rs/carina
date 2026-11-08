//! Text supplied by providers for host diagnostics.

pub mod boundary;
mod composition_contract;
#[cfg(test)]
mod composition_tests;
mod guest_inputs;
#[cfg(feature = "guest-runtime")]
mod guest_store;
mod permissions;
#[cfg(test)]
mod round7_tests;
pub use permissions::{InvalidRequiredPermission, RequiredPermission};
mod saved_configuration;
pub use guest_inputs::{GuestCredentialHandoff, GuestInputCall, GuestInputInventory};
#[cfg(feature = "guest-runtime")]
pub use guest_store::{GuestInputStore, GuestStoreData};
pub use saved_configuration::{
    SavedInputsPending, SavedInputsResolved, SavedProviderConfiguration,
};

#[cfg(test)]
thread_local! { static REDACTION_PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// RedactionSet fields remain private.
///
/// ```compile_fail,E0451
/// use carina_core::provider_text::RedactionSet;
/// let _ = RedactionSet { spellings: Default::default(), generation: Default::default(), values: Default::default() };
/// ```
/// RedactionSet has no default constructor.
///
/// ```compile_fail,E0277
/// let _: carina_core::provider_text::RedactionSet = Default::default();
/// ```
/// Redacted text cannot be constructed externally.
///
/// ```compile_fail,E0603
/// let _ = carina_core::provider_text::RedactedProviderText(String::new());
/// ```
/// Redacted text has no String conversion.
///
/// ```compile_fail,E0277
/// let _: carina_core::provider_text::RedactedProviderText = String::new().into();
/// ```
/// Sessions are minted by core.
///
/// ```compile_fail,E0451
/// use carina_core::provider::ProviderSession;
/// let _ = ProviderSession { config: None, provider: String::new(), binding: None, secrets: todo!() };
/// ```
/// Boundaries cannot be constructed externally.
///
/// ```compile_fail,E0451
/// use carina_core::provider::ProviderBoundary;
/// let _ = ProviderBoundary { provider: todo!(), normalizer: todo!(), secrets: todo!(), schemas: todo!() };
/// ```
/// Unbound provider errors have no Display.
///
/// ```compile_fail,E0277
/// fn display(error: carina_core::provider::ProviderError) { let _ = format!("{error}"); }
/// ```
/// Unbound provider errors have no to_string.
///
/// ```compile_fail,E0599
/// fn display(error: carina_core::provider::ProviderError) { let _ = error.to_string(); }
/// ```
/// ProviderText has no Display.
///
/// ```compile_fail,E0277
/// fn display(text: carina_core::provider_text::ProviderText) { let _ = format!("{text}"); }
/// ```
/// ProviderText has no to_string.
///
/// ```compile_fail,E0599
/// fn display(text: carina_core::provider_text::ProviderText) { let _ = text.to_string(); }
/// ```
/// Unbound ErrorText has no Display.
///
/// ```compile_fail,E0277
/// use carina_core::provider_text::{ErrorText, Unbound};
/// fn display(text: ErrorText<Unbound>) { let _ = format!("{text}"); }
/// ```
/// Unbound ErrorText has no to_string.
///
/// ```compile_fail,E0599
/// use carina_core::provider_text::{ErrorText, Unbound};
/// fn display(text: ErrorText<Unbound>) { let _ = text.to_string(); }
/// ```
/// Unbound diagnostics have no Display.
///
/// ```compile_fail,E0277
/// fn display(text: carina_core::provider_text::ProviderDiagnostic) { let _ = format!("{text}"); }
/// ```
/// Partial-read reasons must remain typed.
///
/// ```compile_fail,E0308
/// let _ = carina_core::provider::PartialReadDiagnostic::<carina_core::provider_text::Unbound>::new(String::from("provider reason"), vec!["name".into()].try_into().unwrap());
/// ```
/// Test features cannot grant an empty-session provider bypass.
///
/// ```compile_fail,E0624
/// fn bypass(provider: Box<dyn carina_core::provider::Provider>) {
///     let _ = carina_core::provider::InitializedProvider::for_test(provider, carina_core::provider::ReferenceScope::Unavailable);
/// }
/// ```
#[doc(hidden)]
pub struct BoundaryApiProbes;

/// A provider session requires core's whole-plan admission witness.
///
/// ```compile_fail,E0624
/// use carina_core::provider::{ProviderPlan, ProviderSession};
/// fn slices(plan: &ProviderPlan) {
///     let _ = ProviderSession::new(plan, "mock", None, None);
/// }
/// ```
#[doc(hidden)]
pub struct ProviderSessionSliceProbe;

use std::borrow::Borrow;
use std::fmt;
use std::ops::Deref;

use carina_provider_protocol::is_default_ignorable;
use serde::{Deserialize, Deserializer, Serialize};

/// Maximum stored diagnostic size, including a truncation marker.
pub const MAX_PROVIDER_TEXT_BYTES: usize = 8 * 1024;

mod state_sealed {
    use super::{Diagnostic, ErrorText, TextState};

    pub trait Sealed: Sized {
        fn operations() -> Operations<Self>
        where
            Self: TextState;
    }

    // The public sealed supertrait can reveal this opaque helper, including
    // through generic TextState bounds, but only core can call its operations.
    pub struct Operations<S: TextState> {
        pub(super) message: fn(S::Message) -> ErrorText<S>,
        pub(super) context: fn(S::Context) -> ErrorText<S>,
        pub(super) partial_read_reason: fn(S::PartialReadReason) -> Diagnostic<S>,
    }
    impl<S: TextState> Operations<S> {
        pub(crate) fn message(&self, text: S::Message) -> ErrorText<S> {
            (self.message)(text)
        }
        pub(crate) fn context(&self, text: S::Context) -> ErrorText<S> {
            (self.context)(text)
        }
        pub(crate) fn partial_read_reason(&self, reason: S::PartialReadReason) -> Diagnostic<S> {
            (self.partial_read_reason)(reason)
        }
    }
}

/// Whether diagnostic text has crossed the host ingestion boundary.
pub trait TextState:
    state_sealed::Sealed + Clone + fmt::Debug + Default + PartialEq + Eq + Send + Sync + 'static
{
    #[doc(hidden)]
    const BOUND: bool;
    type Message: From<String> + for<'a> From<&'a str>;
    type Context;
    type PartialReadReason;
    type Permission;
}
/// Provider-owned text, with no host display/storage authority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Unbound;
/// Text authored by the host or redacted at its provider boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bound;
impl state_sealed::Sealed for Unbound {
    fn operations() -> state_sealed::Operations<Self> {
        state_sealed::Operations {
            message: ErrorText::from_provider_text,
            context: |text| ErrorText::from_provider_text(text.0),
            partial_read_reason: |reason| reason,
        }
    }
}
impl state_sealed::Sealed for Bound {
    fn operations() -> state_sealed::Operations<Self> {
        state_sealed::Operations {
            message: ErrorText::new,
            context: ErrorText::new,
            partial_read_reason: |reason| Diagnostic::message_for_state(ErrorText::new(reason)),
        }
    }
}
impl TextState for Unbound {
    const BOUND: bool = false;
    type Message = ProviderText;
    type Context = ProviderContext;
    type PartialReadReason = ProviderDiagnostic;
    type Permission = String;
}
impl TextState for Bound {
    const BOUND: bool = true;
    type Message = String;
    type Context = &'static str;
    type PartialReadReason = String;
    type Permission = RequiredPermission;
}

pub type ProviderErrorText = ErrorText<Unbound>;
pub type HostErrorText = ErrorText<Bound>;
pub type ProviderDiagnostic = Diagnostic<Unbound>;
/// Arbitrary errors cannot be promoted to bound host diagnostics.
///
/// ```compile_fail,E0277
/// use carina_core::provider_text::HostDiagnostic;
/// let cause = HostDiagnostic::new(std::io::Error::other("guest password"));
/// ```
pub type HostDiagnostic = Diagnostic<Bound>;

/// Untrusted provider output. Formatting deliberately reveals no contents.
/// Projection belongs to the provider instance's core ingestion boundary.
///
/// ```compile_fail,E0624
/// use carina_core::provider_text::{ProviderText, RedactionSet};
/// fn bypass(text: ProviderText, secrets: &RedactionSet) {
///     let _ = text.redact(secrets);
/// }
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::provider_text::ProviderText;
/// let text = ProviderText::new("provider output");
/// let raw: &str = text.as_str(); // no plaintext accessor, Deref or Serialize
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProviderText(Vec<OpaquePart>);
#[derive(Clone, PartialEq, Eq)]
enum OpaquePart {
    Text(String),
    Status(u16),
    Protected {
        text: ProviderText,
        secrets: RedactionSet,
    },
}
/// Opaque context for a cause-bearing provider error.
/// Runtime context must be explicitly wrapped in [`ProviderText`].
#[derive(Clone, Debug)]
pub struct ProviderContext(ProviderText);
impl From<&'static str> for ProviderContext {
    fn from(text: &'static str) -> Self {
        Self(ProviderText::new(text))
    }
}
impl From<ProviderText> for ProviderContext {
    fn from(text: ProviderText) -> Self {
        Self(text)
    }
}

/// Explicit retry policy. Error kinds and rendered text do not infer this policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Retryability {
    #[default]
    Unclassified,
    Permanent,
    Transient,
}

/// Attach SDK-supplied retry policy while preserving the complete diagnostic.
pub struct ClassifiedSource<C> {
    cause: C,
    retryability: Retryability,
}
impl<C: DiagnosticSource<Unbound>> ClassifiedSource<C> {
    pub fn new(cause: C, retryability: Retryability) -> Self {
        Self {
            cause,
            retryability,
        }
    }
}
impl<C: DiagnosticSource<Unbound>> diagnostic_source_sealed::Sealed<Unbound>
    for ClassifiedSource<C>
{
}
impl<C: DiagnosticSource<Unbound>> DiagnosticSource<Unbound> for ClassifiedSource<C> {
    fn into_diagnostic(self) -> ProviderDiagnostic {
        let mut diagnostic = self.cause.into_diagnostic();
        diagnostic.retryability = self.retryability;
        diagnostic
    }
}

impl ProviderText {
    fn collect_context(&self, context: &mut RedactionContext) {
        for part in &self.0 {
            if let OpaquePart::Protected { text, secrets } = part {
                context.include(secrets);
                text.collect_context(context);
            }
        }
    }
    pub fn new(text: impl Into<String>) -> Self {
        Self(vec![OpaquePart::Text(text.into())])
    }
    /// Retain a live inventory with opaque text without granting display or
    /// binding authority to the caller.
    pub fn with_redaction(self, secrets: &RedactionSet) -> Self {
        Self(vec![OpaquePart::Protected {
            text: self,
            secrets: secrets.clone(),
        }])
    }
    fn redact(&self, secrets: &RedactionSet) -> RedactedProviderText {
        #[cfg(test)]
        REDACTION_PASSES.with(|count| count.set(count.get() + 1));
        let mut text = String::new();
        let mut stream = RedactionStream::new(secrets.clone());
        for part in &self.0 {
            match part {
                OpaquePart::Text(raw) => stream.push_str(raw, &mut |chunk| text.push_str(chunk)),
                OpaquePart::Status(status) => {
                    stream.flush(&mut |chunk| text.push_str(chunk));
                    text.push_str(&status.to_string());
                }
                OpaquePart::Protected {
                    text: raw,
                    secrets: carried,
                } => {
                    stream.flush(&mut |chunk| text.push_str(chunk));
                    let union = RedactionSet::before_request();
                    union.extend(secrets);
                    union.extend(carried);
                    text.push_str(raw.redact(&union).as_str());
                }
            }
        }
        stream.flush(&mut |chunk| text.push_str(chunk));
        RedactedProviderText(text)
    }
    pub(crate) fn has_throttling_marker(&self) -> bool {
        self.0.iter().any(|part| match part {
            OpaquePart::Text(text) => crate::provider::has_throttling_marker(text),
            OpaquePart::Protected { text, .. } => text.has_throttling_marker(),
            OpaquePart::Status(_) => false,
        })
    }
    pub(crate) fn has_delete_dependency_marker(&self) -> bool {
        self.0.iter().any(|part| match part {
            OpaquePart::Text(text) => crate::provider::has_delete_dependency_marker(text),
            OpaquePart::Protected { text, .. } => text.has_delete_dependency_marker(),
            OpaquePart::Status(_) => false,
        })
    }
    pub fn with_context(&self, context: impl Into<ProviderContext>) -> Self {
        let mut text = context.into().0;
        text.append_host(": ");
        text.append(self);
        text
    }
    pub fn append(&mut self, text: &ProviderText) {
        for part in &text.0 {
            match part {
                OpaquePart::Text(text) => self.append_host(text),
                OpaquePart::Status(status) => self.append_status(*status),
                part @ OpaquePart::Protected { .. } => {
                    self.0.push(part.clone());
                }
            }
        }
    }
    pub fn append_host(&mut self, text: impl AsRef<str>) {
        if let Some(OpaquePart::Text(raw)) = self.0.last_mut() {
            raw.push_str(text.as_ref());
        } else {
            self.0.push(OpaquePart::Text(text.as_ref().to_owned()));
        }
    }
    // Status remains structured even inside an opaque nested cause.
    pub(crate) fn append_status(&mut self, status: u16) {
        self.0.push(OpaquePart::Status(status));
    }
    #[cfg(target_arch = "wasm32")]
    fn wire_text(&self) -> String {
        self.0
            .iter()
            .map(|part| match part {
                OpaquePart::Text(text) => text.clone(),
                OpaquePart::Status(status) => status.to_string(),
                OpaquePart::Protected { text, secrets } => text.redact(secrets).as_str().to_owned(),
            })
            .collect()
    }
}

impl From<String> for ProviderText {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}
impl From<&String> for ProviderText {
    fn from(text: &String) -> Self {
        Self::new(text)
    }
}
impl From<&str> for ProviderText {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}
impl fmt::Debug for ProviderText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider text>")
    }
}

/// Text authored by the host. Provider ingestion never preserves this authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostText(String);
impl From<String> for HostText {
    fn from(text: String) -> Self {
        Self(text)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Segment {
    Host(HostText),
    Provider {
        text: ProviderText,
        secrets: RedactionSet,
    },
}
#[derive(Clone, PartialEq, Eq)]
enum TextContent {
    Unbound(ProviderText),
    Bound(Vec<Segment>),
}

/// Retain live handles for every boundary crossed by a nested diagnostic.
/// Typed cause views must carry these too, rather than selecting one set.
#[derive(Clone, Default)]
pub(crate) struct RedactionContext(Vec<RedactionSet>);
impl RedactionContext {
    pub(crate) fn include(&mut self, set: &RedactionSet) {
        if !self.0.iter().any(|known| known.shares_inventory(set)) {
            self.0.push(set.clone());
        }
    }
    fn with(mut self, set: &RedactionSet) -> Self {
        self.include(set);
        self
    }
    pub(crate) fn extend(&mut self, other: &Self) {
        for set in &other.0 {
            self.include(set);
        }
    }
    fn protect(&self, mut text: ProviderText) -> ProviderText {
        for secrets in &self.0 {
            text = ProviderText(vec![OpaquePart::Protected {
                text,
                secrets: secrets.clone(),
            }]);
        }
        text
    }
    fn generations(&self) -> Vec<u64> {
        self.0.iter().map(RedactionSet::generation).collect()
    }
    fn snapshot(&self) -> Option<RedactionSet> {
        let first = self.0.first()?;
        if self.0.len() == 1 {
            return Some(first.clone());
        }
        let union = RedactionSet::before_request();
        for set in &self.0 {
            union.extend(set);
        }
        Some(union)
    }
}

/// Opaque provider input, or host/provider segments bound to their redaction set.
pub struct ErrorText<S: TextState = Unbound> {
    content: TextContent,
    projection: std::sync::Mutex<Option<ProjectionCache>>,
    state: std::marker::PhantomData<S>,
}
struct ProjectionCache {
    generations: Vec<u64>,
    projection: std::sync::Arc<TextProjection>,
}
#[derive(Clone)]
struct TextProjection {
    text: String,
    diagnostic: DiagnosticText,
}
impl<S: TextState> Clone for ErrorText<S> {
    fn clone(&self) -> Self {
        Self {
            content: self.content.clone(),
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
}
impl<S: TextState> fmt::Debug for ErrorText<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if S::BOUND {
            self.projection().diagnostic.fmt(f)
        } else {
            f.write_str("<provider text>")
        }
    }
}
impl<S: TextState> Default for ErrorText<S> {
    fn default() -> Self {
        Self::new("")
    }
}
impl<S: TextState> PartialEq for ErrorText<S> {
    fn eq(&self, other: &Self) -> bool {
        if S::BOUND {
            self.redacted_str() == other.redacted_str()
        } else {
            self.content == other.content
        }
    }
}
impl<S: TextState> Eq for ErrorText<S> {}
impl<S: TextState> ErrorText<S> {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            content: if S::BOUND {
                TextContent::Bound(vec![Segment::Host(HostText(text))])
            } else {
                TextContent::Unbound(ProviderText::new(text))
            },
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
    fn projection(&self) -> std::sync::Arc<TextProjection> {
        let generations = self.redaction_context().generations();
        let mut slot = self
            .projection
            .lock()
            .expect("projection cache lock poisoned");
        if slot
            .as_ref()
            .is_none_or(|cached| cached.generations != generations)
        {
            let mut text = String::new();
            match &self.content {
                TextContent::Unbound(_) => text.push_str("<provider text>"),
                TextContent::Bound(segments) => {
                    for segment in segments {
                        match segment {
                            Segment::Host(host) => text.push_str(&host.0),
                            Segment::Provider { text: raw, secrets } => {
                                text.push_str(secrets.redact(raw).as_str())
                            }
                        }
                    }
                }
            }
            *slot = Some(ProjectionCache {
                generations,
                projection: std::sync::Arc::new(TextProjection {
                    diagnostic: DiagnosticText::new(&text),
                    text,
                }),
            });
        }
        slot.as_ref()
            .expect("projection initialized")
            .projection
            .clone()
    }
    #[cfg(test)]
    fn cached_projection_count(&self) -> usize {
        usize::from(self.projection.lock().unwrap().is_some())
    }
    pub(crate) fn redacted_str(&self) -> String {
        self.projection().text.clone()
    }
    pub(crate) fn opaque(&self) -> ProviderText {
        match &self.content {
            TextContent::Unbound(text) => text.clone(),
            TextContent::Bound(segments) => {
                let mut text = ProviderText::default();
                for segment in segments {
                    match segment {
                        Segment::Host(host) => text.append_host(&host.0),
                        Segment::Provider { text: raw, secrets } => {
                            text.0.push(OpaquePart::Protected {
                                text: raw.clone(),
                                secrets: secrets.clone(),
                            });
                        }
                    }
                }
                text
            }
        }
    }
    pub(crate) fn redaction_context(&self) -> RedactionContext {
        let mut context = RedactionContext::default();
        match &self.content {
            TextContent::Unbound(text) => text.collect_context(&mut context),
            TextContent::Bound(segments) => {
                for segment in segments {
                    if let Segment::Provider { text, secrets } = segment {
                        context.include(secrets);
                        text.collect_context(&mut context);
                    }
                }
            }
        }
        context
    }
    pub(crate) fn with_redaction_context(mut self, context: &RedactionContext) -> Self {
        if context.0.is_empty() {
            return self;
        }
        match &mut self.content {
            TextContent::Unbound(text) => *text = context.protect(text.clone()),
            TextContent::Bound(segments) => {
                for segment in segments {
                    if let Segment::Provider { text, .. } = segment {
                        *text = context.protect(text.clone());
                    }
                }
            }
        }
        self.projection = Default::default();
        self
    }
    pub(crate) fn protect_opaque(text: ProviderText, context: &RedactionContext) -> ProviderText {
        context.protect(text)
    }
    pub(crate) fn known_secrets(&self) -> Option<RedactionSet> {
        self.redaction_context().snapshot()
    }
    pub(crate) fn append_host(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.projection = Default::default();
        match &mut self.content {
            TextContent::Unbound(raw) => raw.append_host(text),
            TextContent::Bound(segments) => segments.push(Segment::Host(HostText(text))),
        }
    }
    pub(crate) fn append(&mut self, other: &Self) {
        self.projection = Default::default();
        match (&mut self.content, &other.content) {
            (TextContent::Bound(segments), TextContent::Bound(other)) => {
                segments.extend(other.clone())
            }
            (TextContent::Unbound(raw), _) => raw.append(&other.opaque()),
            _ => unreachable!("text states agree"),
        }
    }
    pub(crate) fn wire_text(&self) -> String {
        #[cfg(target_arch = "wasm32")]
        if !S::BOUND {
            return self.opaque().wire_text();
        }
        self.redacted_str()
    }
}
impl<S: TextState> ErrorText<S> {
    pub(crate) fn from_opaque_for_state(text: ProviderText) -> Self {
        assert!(
            !S::BOUND,
            "an opaque provider cause requires its boundary session"
        );
        Self {
            content: TextContent::Unbound(text),
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
}
impl ErrorText<Bound> {
    pub(crate) fn host_for_state<S: TextState>(&self) -> ErrorText<S> {
        ErrorText {
            content: if S::BOUND {
                self.content.clone()
            } else {
                TextContent::Unbound(self.opaque())
            },
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
}
impl ErrorText<Unbound> {
    pub(crate) fn from_provider_text(text: ProviderText) -> Self {
        Self {
            content: TextContent::Unbound(text),
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
    pub(crate) fn into_host(self, secrets: &RedactionSet) -> ErrorText<Bound> {
        ErrorText {
            content: TextContent::Bound(vec![Segment::Provider {
                text: self.opaque(),
                secrets: secrets.clone(),
            }]),
            projection: Default::default(),
            state: std::marker::PhantomData,
        }
    }
}
impl<S: TextState> From<&str> for ErrorText<S> {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}
impl<S: TextState> From<String> for ErrorText<S> {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}
impl ErrorText<Bound> {
    /// An owned diagnostic snapshot. Later inventory generations can replace
    /// this handle's cache without invalidating a previously returned value.
    pub fn as_diagnostic(&self) -> DiagnosticText {
        self.projection().diagnostic.clone()
    }
}
impl fmt::Display for ErrorText<Bound> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.projection().diagnostic.fmt(f)
    }
}
impl<S: TextState> Serialize for ErrorText<S> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        if !S::BOUND {
            return Err(serde::ser::Error::custom(
                "provider text requires a boundary before serialization",
            ));
        }
        serializer.serialize_str(&self.redacted_str())
    }
}
impl<'de> Deserialize<'de> for ErrorText<Unbound> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Output which passed through the provider-text seam. Its wire form remains
/// a string, including when persisted as a partial-read reason.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RedactedProviderText(String);

impl fmt::Debug for RedactedProviderText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RedactedProviderText")
            .field(&DiagnosticText::new(&self.0).as_str())
            .finish()
    }
}

impl RedactedProviderText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Deref for RedactedProviderText {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for RedactedProviderText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        DiagnosticText::new(&self.0).fmt(f)
    }
}
impl PartialEq<&str> for RedactedProviderText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Unbound provider failures cannot be passed to the host renderer.
///
/// ```compile_fail,E0599
/// use carina_core::provider::{HostError, ProviderError};
/// fn render(error: &HostError) -> String { error.to_string() }
/// let returned = ProviderError::api_error("provider returned text");
/// let leaked = render(&returned);
/// ```
///
/// ```compile_fail,E0599
/// use carina_core::provider::ProviderError;
/// let returned = ProviderError::api_error("provider output");
/// let raw: &str = returned.as_str();
/// ```
///
/// ```compile_fail,E0277
/// use carina_core::provider::ProviderError;
/// let returned = ProviderError::api_error("provider output");
/// let json = serde_json::to_string(&returned).unwrap();
/// ```
///
/// ```compile_fail,E0308
/// use carina_core::provider::{Provider, ProviderReadyDataSource};
/// async fn bypass(provider: &dyn Provider, request: &ProviderReadyDataSource) {
///     carina_core::executor::read_data_source_with_retry(provider, request).await.unwrap();
/// }
/// ```
///
/// Binding an unbound error is private even when a request capability is available.
///
/// ```compile_fail,E0624
/// use carina_core::provider::ProviderError;
/// use carina_core::provider_text::RedactionSet;
/// fn bypass(error: ProviderError, sent: &RedactionSet) {
///     let text = error.with_secrets(sent).to_string();
/// }
/// ```
///
/// Secret plaintexts collected from values actually prepared for a provider.
/// There is no Default, string constructor, deserializer or plaintext accessor.
/// An empty pre-request inventory is private to the host boundary.
///
/// ```compile_fail,E0624
/// use carina_core::provider_text::RedactionSet;
/// let empty = RedactionSet::before_request();
/// ```
///
///
/// ```compile_fail,E0277
/// use carina_core::provider_text::RedactionSet;
/// let _: RedactionSet = vec!["arbitrary".to_owned()].into();
/// ```
#[derive(Clone)]
pub struct RedactionSet {
    spellings: std::sync::Arc<std::sync::RwLock<std::sync::Arc<Vec<String>>>>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    values: std::sync::Arc<std::sync::RwLock<std::collections::HashSet<[u8; 32]>>>,
}
impl PartialEq for RedactionSet {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.spellings, &other.spellings)
            || self.snapshot() == other.snapshot()
    }
}
impl Eq for RedactionSet {}

impl fmt::Debug for RedactionSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RedactionSet(<redacted>)")
    }
}

impl RedactionSet {
    /// Whether two handles observe the same append-only instance inventory.
    pub fn shares_inventory(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.spellings, &other.spellings)
    }
    pub(crate) fn redact(&self, text: &ProviderText) -> RedactedProviderText {
        text.redact(self)
    }
    /// Explicit evidence that the error predates any provider request.
    pub(crate) fn before_request() -> Self {
        Self {
            spellings: std::sync::Arc::default(),
            generation: std::sync::Arc::default(),
            values: std::sync::Arc::default(),
        }
    }

    /// Fixed sent values for fixtures, unavailable in production builds.
    #[cfg(test)]
    pub(crate) fn for_test(values: &[&str]) -> Self {
        let mut secrets = Self::before_request();
        for value in values {
            secrets.insert(value);
        }
        secrets.sort();
        secrets
    }

    fn snapshot(&self) -> std::sync::Arc<Vec<String>> {
        self.versioned_snapshot().1
    }

    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    fn versioned_snapshot(&self) -> (u64, std::sync::Arc<Vec<String>>) {
        let spellings = self
            .spellings
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.generation(), spellings.clone())
    }

    /// Append values before provider ingress. Every clone, including the guest
    /// stderr writer, retains this same inventory for the instance's lifetime.
    pub(crate) fn extend(&self, other: &Self) {
        if std::sync::Arc::ptr_eq(&self.spellings, &other.spellings) {
            return;
        }
        let values = other
            .values
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        self.values
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(values);
        let added = other.snapshot();
        if added.is_empty() {
            return;
        }
        let mut current = self
            .spellings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let added: Vec<_> = added
            .iter()
            .filter(|value| {
                current
                    .binary_search_by(|existing| Self::compare_values(existing, value))
                    .is_err()
            })
            .collect();
        if added.is_empty() {
            return;
        }
        let values = std::sync::Arc::make_mut(&mut current);
        values.extend(added.into_iter().cloned());
        Self::sort_values(values);
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    fn would_exceed_secret_limit(&self, other: &Self, limit: usize) -> bool {
        let mut values = self
            .values
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        values.extend(
            other
                .values
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .copied(),
        );
        values.len() > limit
    }

    fn compare_values(a: &str, b: &str) -> std::cmp::Ordering {
        b.len().cmp(&a.len()).then_with(|| a.cmp(b))
    }
    fn sort_values(values: &mut Vec<String>) {
        values.sort_by(|a, b| Self::compare_values(a, b));
        values.dedup();
    }
    fn sort(&mut self) {
        let mut values = self
            .spellings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::sort_values(std::sync::Arc::make_mut(&mut values));
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    /// The collector is crate-private; public capabilities below supply only
    /// checked provider inputs. The controlled SecretPayload projection is
    /// the sole place where secret contents enter this inventory.
    pub(crate) fn collect<
        'a,
        P: crate::resource::ValueKind + 'a,
        B: crate::resource::BindingAddress + 'a,
    >(
        values: impl IntoIterator<Item = &'a crate::resource::Value<P, B>>,
    ) -> Self {
        use crate::resource::{ConcreteValue, DeferredValue, Value};
        fn visit<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
            value: &Value<P, B>,
            secret: bool,
            out: &mut RedactionSet,
        ) {
            match value {
                Value::Deferred(DeferredValue::Secret(payload)) => {
                    let _: Result<(), std::convert::Infallible> =
                        payload.try_project_plaintext(|value| {
                            visit(value, true, out);
                            Ok(())
                        });
                }
                Value::Concrete(ConcreteValue::List(values)) => {
                    for value in values {
                        visit(value, secret, out);
                    }
                }
                Value::Concrete(ConcreteValue::Map(values)) => {
                    for (key, value) in values {
                        if secret {
                            out.insert(key);
                        }
                        visit(value, secret, out);
                    }
                }
                Value::Concrete(ConcreteValue::String(text))
                    if secret || text.is_diagnostic_sensitive() =>
                {
                    out.insert(text.expose_plaintext())
                }
                Value::Concrete(ConcreteValue::StringList(values)) => {
                    for value in values {
                        if secret || value.is_diagnostic_sensitive() {
                            out.insert(value.expose_plaintext());
                        }
                    }
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(value)) if secret => {
                    out.insert(value.api_value())
                }
                Value::Concrete(ConcreteValue::Int(value)) if secret => {
                    out.insert(&value.to_string())
                }
                Value::Concrete(ConcreteValue::Float(value)) if secret => {
                    out.insert(&value.to_string());
                    out.insert(&serde_json::to_string(value).expect("finite float encodes"));
                }
                Value::Concrete(ConcreteValue::Bool(value)) if secret => {
                    out.insert(&value.to_string())
                }
                Value::Concrete(ConcreteValue::Duration(value)) if secret => {
                    out.insert(&value.as_secs().to_string())
                }
                _ => {}
            }
        }
        let mut result = Self::before_request();
        for value in values {
            visit(value, false, &mut result);
        }
        result.sort();
        result
    }

    fn insert(&mut self, plaintext: &str) {
        if plaintext.is_empty() {
            return;
        }
        use sha2::{Digest, Sha256};
        self.values
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(Sha256::digest(plaintext.as_bytes()).into());
        let mut locked = self
            .spellings
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let spellings = std::sync::Arc::make_mut(&mut locked);
        for spelling in SecretSpelling::ALL {
            spellings.push(spelling.encode(plaintext));
        }
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }
}

macro_rules! secret_spellings {
    ($(#[$doc:meta])* enum $name:ident { $($variant:ident),+ $(,)? }) => {
        $(#[$doc])*
        #[derive(Clone, Copy)]
        enum $name { $($variant),+ }
        impl $name { const ALL: &'static [Self] = &[$(Self::$variant),+]; }
    };
}
secret_spellings! {
/// Covered: raw text; JSON string escaping without surrounding quotes; DSL
/// double-quoted escaping; the DSL's selected string-literal escaping without
/// surrounding quotes; terminal control-character escaping; RFC 3986 percent
/// encoding of UTF-8 bytes with uppercase or lowercase hexadecimal digits.
/// Not covered: base64, hashes, encryption, compression, mixed-case percent
/// encoding, partial or repeated encoding, or other transformations that do
/// not contain a complete covered spelling as a substring.
enum SecretSpelling {
    Raw,
    JsonEscaped,
    DslDoubleQuoted,
    DslLiteral,
    TerminalEscaped,
    PercentEncodedUpper,
    PercentEncodedLower,
}
}
impl SecretSpelling {
    fn encode(self, plaintext: &str) -> String {
        match self {
            Self::Raw => plaintext.to_owned(),
            Self::JsonEscaped => {
                let json = serde_json::to_string(plaintext).expect("strings encode as JSON");
                json[1..json.len() - 1].to_owned()
            }
            Self::DslDoubleQuoted => crate::parser::display_double_quoted_literal_part(plaintext),
            Self::DslLiteral => {
                let literal = crate::parser::display_string_literal(plaintext);
                literal[1..literal.len() - 1].to_owned()
            }
            Self::TerminalEscaped => plaintext
                .chars()
                .flat_map(|ch| match ch {
                    '\0'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {
                        ch.escape_unicode().collect::<Vec<_>>()
                    }
                    ch => vec![ch],
                })
                .collect(),
            Self::PercentEncodedUpper => Self::percent_encode(plaintext, b"0123456789ABCDEF"),
            Self::PercentEncodedLower => Self::percent_encode(plaintext, b"0123456789abcdef"),
        }
    }
    fn percent_encode(plaintext: &str, hex: &[u8; 16]) -> String {
        let mut encoded = String::new();
        for byte in plaintext.bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                encoded.push(char::from(byte));
            } else {
                encoded.push('%');
                encoded.push(char::from(hex[usize::from(byte >> 4)]));
                encoded.push(char::from(hex[usize::from(byte & 15)]));
            }
        }
        encoded
    }
}

/// Retain at most a secret's lookahead plus a compacted prefix. Coverage and
/// continuation bits merge overlapping matches, while adjacent occurrences
/// keep separate masks. Delaying output prevents a later, longer match from
/// exposing any prefix or suffix of an earlier overlapping match.
struct RedactionStream {
    #[cfg(any(test, feature = "test-utils"))]
    refresh_checks: usize,
    #[cfg(any(test, feature = "test-utils"))]
    snapshot_locks: usize,
    generation: u64,
    secrets: RedactionSet,
    spellings: std::sync::Arc<Vec<String>>,
    lookahead: usize,
    buffer: String,
    covered: Vec<(bool, bool)>,
    offset: usize,
}
impl RedactionStream {
    fn new(secrets: RedactionSet) -> Self {
        let (generation, spellings) = secrets.versioned_snapshot();
        let lookahead = spellings.iter().map(String::len).max().unwrap_or(1);
        Self {
            #[cfg(any(test, feature = "test-utils"))]
            refresh_checks: 0,
            #[cfg(any(test, feature = "test-utils"))]
            snapshot_locks: 1,
            generation,
            secrets,
            spellings,
            lookahead,
            buffer: String::new(),
            covered: Vec::new(),
            offset: 0,
        }
    }
    fn refresh(&mut self) {
        #[cfg(any(test, feature = "test-utils"))]
        {
            self.refresh_checks += 1;
        }
        if self.generation == self.secrets.generation() {
            return;
        }
        #[cfg(any(test, feature = "test-utils"))]
        {
            self.snapshot_locks += 1;
        }
        let (generation, spellings) = self.secrets.versioned_snapshot();
        self.generation = generation;
        self.lookahead = spellings.iter().map(String::len).max().unwrap_or(1);
        // Recheck retained lookahead if another operation added a spelling.
        for (end, ch) in self.buffer.char_indices() {
            let end = end + ch.len_utf8();
            for secret in spellings.iter() {
                if self.buffer[..end].ends_with(secret) {
                    let start = end - secret.len();
                    for (index, (covered, continuation)) in
                        self.covered[start..end].iter_mut().enumerate()
                    {
                        *covered = true;
                        *continuation |= index > 0;
                    }
                }
            }
        }
        self.spellings = spellings;
    }
    fn push_str(&mut self, text: &str, output: &mut impl FnMut(&str)) {
        self.refresh();
        for ch in text.chars() {
            self.buffer.push(ch);
            self.covered
                .extend(std::iter::repeat_n((false, false), ch.len_utf8()));
            for secret in self.spellings.iter() {
                if self.buffer.ends_with(secret) {
                    let start = self.buffer.len() - secret.len();
                    for (index, (covered, continuation)) in
                        self.covered[start..].iter_mut().enumerate()
                    {
                        *covered = true;
                        *continuation |= index > 0;
                    }
                }
            }
            while self.buffer.len() - self.offset >= self.lookahead {
                self.emit_char(output);
            }
            if self.offset >= 4096 && self.offset >= self.buffer.len() / 2 {
                self.buffer.drain(..self.offset);
                self.covered.drain(..self.offset);
                self.offset = 0;
            }
        }
    }
    /// Live writes may release every byte that cannot start a future match.
    /// Buffered diagnostic assembly keeps its lookahead until final projection,
    /// including while its append-only input inventory is still growing.
    fn emit_unambiguous(&mut self, output: &mut impl FnMut(&str)) {
        let pending = &self.buffer[self.offset..];
        let retained = pending
            .char_indices()
            .find_map(|(start, _)| {
                let suffix = &pending[start..];
                self.spellings
                    .iter()
                    .any(|secret| suffix.len() < secret.len() && secret.starts_with(suffix))
                    .then_some(suffix.len())
            })
            .unwrap_or(0);
        let end = self.buffer.len() - retained;
        while self.offset < end {
            self.emit_char(output);
        }
        if self.offset == self.buffer.len() {
            self.buffer.clear();
            self.covered.clear();
            self.offset = 0;
        }
    }
    fn emit_char(&mut self, output: &mut impl FnMut(&str)) {
        let length = self.buffer[self.offset..]
            .chars()
            .next()
            .expect("pending character")
            .len_utf8();
        let (covered, continuation) = self.covered[self.offset];
        if !covered {
            output(&self.buffer[self.offset..self.offset + length]);
        } else if !continuation {
            output(crate::schema::SECRET_VALUE_MASK);
        }
        self.offset += length;
    }
    fn flush(&mut self, output: &mut impl FnMut(&str)) {
        self.refresh();
        self.emit_pending(output);
    }
    fn emit_pending(&mut self, output: &mut impl FnMut(&str)) {
        while self.offset < self.buffer.len() {
            self.emit_char(output);
        }
        self.buffer.clear();
        self.covered.clear();
        self.offset = 0;
    }
}
/// Redact guest stderr before giving complete, bounded lines to the sink.
/// No display capability or provider text escapes this writer.
pub struct RedactingLineWriter<W: std::io::Write> {
    stream: RedactionStream,
    line: DiagnosticTextBuilder,
    pending: bool,
    sink: W,
}
impl<W: std::io::Write> RedactingLineWriter<W> {
    #[cfg(any(test, feature = "test-utils"))]
    pub fn redaction_stats_for_test(&self) -> (usize, usize) {
        (self.stream.refresh_checks, self.stream.snapshot_locks)
    }
    pub fn new(secrets: &RedactionSet, sink: W) -> Self {
        Self {
            stream: RedactionStream::new(secrets.clone()),
            line: DiagnosticTextBuilder::default(),
            pending: false,
            sink,
        }
    }
    /// Metadata loading sends no configuration or desired payload to the guest.
    pub fn for_metadata(sink: W) -> Self {
        Self::new(&RedactionSet::before_request(), sink)
    }
    pub fn push_str(&mut self, text: &str) -> std::io::Result<()> {
        let Self {
            stream,
            line,
            pending,
            sink,
        } = self;
        let mut result = Ok(());
        stream.push_str(text, &mut |chunk| {
            if result.is_ok() {
                result = Self::append(line, pending, sink, chunk);
            }
        });
        stream.emit_unambiguous(&mut |chunk| {
            if result.is_ok() {
                result = Self::append(line, pending, sink, chunk);
            }
        });
        result
    }
    pub fn finish(&mut self) -> std::io::Result<()> {
        let Self {
            stream,
            line,
            pending,
            sink,
        } = self;
        let mut result = Ok(());
        stream.flush(&mut |chunk| {
            if result.is_ok() {
                result = Self::append(line, pending, sink, chunk);
            }
        });
        result?;
        if *pending {
            sink.write_all(std::mem::take(line).finish().as_bytes())?;
            sink.write_all(b"\n")?;
            *pending = false;
        }
        sink.flush()
    }
    fn append(
        line: &mut DiagnosticTextBuilder,
        pending: &mut bool,
        sink: &mut W,
        text: &str,
    ) -> std::io::Result<()> {
        for part in text.split_inclusive('\n') {
            let content = part.strip_suffix('\n').unwrap_or(part);
            line.push_str(content);
            *pending |= !content.is_empty();
            if part.ends_with('\n') {
                sink.write_all(std::mem::take(line).finish().as_bytes())?;
                sink.write_all(b"\n")?;
                sink.flush()?;
                *pending = false;
            }
        }
        Ok(())
    }
}

pub(crate) struct ErrorWireText<'a, S: TextState> {
    pub message: &'a ErrorText<S>,
    pub cause: Option<&'a Diagnostic<S>>,
    pub provider_name: Option<&'a ErrorText<S>>,
    pub operation: Option<&'a ErrorText<S>>,
    pub code: Option<&'a ErrorText<S>>,
    pub request_id: Option<&'a ErrorText<S>>,
}
impl<S: TextState> ErrorWireText<'_, S> {
    pub(crate) fn write(self, wire: &mut carina_provider_protocol::types::ProviderError) {
        wire.message = self.message.wire_text();
        wire.cause = self.cause.map(|cause| cause.text.wire_text());
        wire.provider_name = self.provider_name.map(ErrorText::wire_text);
        wire.operation = self.operation.map(ErrorText::wire_text);
        wire.code = self.code.map(ErrorText::wire_text);
        wire.request_id = self.request_id.map(ErrorText::wire_text);
    }
}
impl<S: TextState> ErrorText<S> {
    pub(crate) fn partial_read_wire(
        &self,
        missing_attributes: Vec<String>,
    ) -> carina_provider_protocol::types::PartialReadDiagnostic {
        carina_provider_protocol::types::PartialReadDiagnostic {
            reason: self.wire_text(),
            missing_attributes,
        }
    }
}

/// Provider-supplied data with a safe diagnostic representation.
///
/// Matching, lookup and serialization use the original value through explicit
/// accessors. Formatting always passes through [`DiagnosticText`].
#[derive(Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderKey(String);

impl ProviderKey {
    /// Original data for semantic operations; use Display for diagnostics.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the wrapper without changing the original data.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for ProviderKey {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for ProviderKey {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        DiagnosticText::new(&self.0).fmt(f)
    }
}

impl fmt::Debug for ProviderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ProviderKey")
            .field(&DiagnosticText::new(&self.0).as_str())
            .finish()
    }
}

impl PartialEq<str> for ProviderKey {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for ProviderKey {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for ProviderKey {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

/// Provider diagnostic text, sanitized before it can be displayed.
/// Use [`ProviderKey`] when the original string also has semantic meaning.
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DiagnosticText(String);

impl DiagnosticText {
    pub fn new(text: impl Into<String>) -> Self {
        let mut builder = DiagnosticTextBuilder::default();
        builder.push_str(&text.into());
        builder.finish()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Incremental form of the same sanitizer, retaining only a bounded prefix.
/// The final marker includes bytes omitted across every supplied chunk.
#[derive(Default)]
pub struct DiagnosticTextBuilder {
    prefix: String,
    sanitized_bytes: usize,
    truncated: bool,
}

impl DiagnosticTextBuilder {
    pub fn push_str(&mut self, text: &str) {
        self.push_text(text, false);
    }

    /// Append an inline diagnostic field without allowing embedded line breaks.
    pub(crate) fn push_inline_str(&mut self, text: &str) {
        self.push_text(text, true);
    }

    fn push_text(&mut self, text: &str, inline: bool) {
        for c in text.chars() {
            match c {
                '\n' | '\t' if !inline => self.push_sanitized_char(c),
                c if inline && is_default_ignorable(c) => {
                    for escaped in c.escape_unicode() {
                        self.push_sanitized_char(escaped);
                    }
                }
                c if inline && (c.is_control() || matches!(c, '\u{2028}' | '\u{2029}')) => {
                    for escaped in c.escape_debug() {
                        self.push_sanitized_char(escaped);
                    }
                }
                c if c.is_control() => {
                    for escaped in c.escape_unicode() {
                        self.push_sanitized_char(escaped);
                    }
                }
                c => self.push_sanitized_char(c),
            }
        }
    }

    fn push_sanitized_char(&mut self, c: char) {
        self.sanitized_bytes = self.sanitized_bytes.saturating_add(c.len_utf8());
        if !self.truncated && self.prefix.len() + c.len_utf8() <= MAX_PROVIDER_TEXT_BYTES {
            self.prefix.push(c);
        } else {
            self.truncated = true;
        }
    }

    pub(crate) fn retained_len(&self) -> usize {
        self.prefix.len()
    }

    pub fn finish(self) -> DiagnosticText {
        self.finish_with_prefix_len().0
    }

    pub(crate) fn finish_with_prefix_len(mut self) -> (DiagnosticText, usize) {
        let mut retained = self.prefix.len();
        if self.sanitized_bytes > MAX_PROVIDER_TEXT_BYTES {
            let mut end = self.prefix.len();
            loop {
                let marker = format!("… (truncated, {} bytes)", self.sanitized_bytes - end);
                if end + marker.len() <= MAX_PROVIDER_TEXT_BYTES {
                    retained = end;
                    self.prefix.truncate(end);
                    self.prefix.push_str(&marker);
                    break;
                }
                end = MAX_PROVIDER_TEXT_BYTES - marker.len();
                while !self.prefix.is_char_boundary(end) {
                    end -= 1;
                }
            }
        }
        (DiagnosticText(self.prefix), retained)
    }
}

impl From<String> for DiagnosticText {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&str> for DiagnosticText {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

// Borrowing exposes only the already sanitized, immutable text.
impl Deref for DiagnosticText {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for DiagnosticText {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<String> for DiagnosticText {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for DiagnosticText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<str> for DiagnosticText {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl<'de> Deserialize<'de> for DiagnosticText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl fmt::Display for DiagnosticText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn frame_downcast<'a, T: std::error::Error + 'static>(
    error: &'a (dyn std::error::Error + 'static),
) -> Option<&'a T> {
    error
        .downcast_ref::<T>()
        .or_else(|| error.downcast_ref::<std::sync::Arc<T>>().map(AsRef::as_ref))
        .or_else(|| error.downcast_ref::<Box<T>>().map(AsRef::as_ref))
}
enum OpaqueFrame<'a> {
    Provider(&'a crate::provider::ProviderError),
    Host(&'a crate::provider::HostError),
    Diagnostic(&'a HostDiagnostic),
    Validation(&'a crate::schema::ProviderValidation),
    ValidationCategory(&'a crate::schema::TypeError),
    Dispatch(&'a crate::provider::CustomTypeDispatchError),
}
fn opaque_frame<'a>(error: &'a (dyn std::error::Error + 'static)) -> Option<OpaqueFrame<'a>> {
    if let Some(error) = frame_downcast::<OpaqueProviderFailure>(error) {
        return Some(OpaqueFrame::Provider(&error.0));
    }
    if let Some(error) = frame_downcast::<crate::provider::HostError>(error) {
        return Some(OpaqueFrame::Host(error));
    }
    if let Some(error) = frame_downcast::<crate::schema::ProviderValidation>(error) {
        return Some(OpaqueFrame::Validation(error));
    }
    if let Some(error) = frame_downcast::<crate::schema::TypeError>(error)
        && (error.provider_validation_reason().is_some()
            || error.provider_validation_child().is_some())
    {
        return Some(OpaqueFrame::ValidationCategory(error));
    }
    if let Some(error) = frame_downcast::<crate::provider::CustomTypeDispatchError>(error) {
        return Some(OpaqueFrame::Dispatch(error));
    }
    frame_downcast::<HostDiagnostic>(error).map(OpaqueFrame::Diagnostic)
}
impl<'a> OpaqueFrame<'a> {
    fn text<S: TextState>(&self) -> ErrorText<S> {
        match self {
            Self::Provider(error) => ErrorText::from_opaque_for_state(error.diagnostic_text()),
            Self::Host(error) => error.segmented_text().host_for_state(),
            Self::Diagnostic(error) => error.text.host_for_state(),
            Self::Validation(error) => error.capture_text(),
            Self::ValidationCategory(error) => error.provider_diagnostic_text(),
            Self::Dispatch(error) => {
                if let crate::provider::CustomTypeDispatchError::Validation(category) = error {
                    if let crate::schema::TypeError::ValidationFailed { message } =
                        category.as_ref()
                    {
                        return message.capture_text();
                    }
                    return Diagnostic::capture(category.as_ref());
                }
                Diagnostic::capture_foreign(*error)
            }
        }
    }
    fn retryability(&self) -> Retryability {
        match self {
            Self::Provider(error) => error.retryability(),
            Self::Host(error) => error.retryability(),
            Self::Diagnostic(error) => error.retryability(),
            Self::Validation(error) => error.retryability(),
            Self::ValidationCategory(error) => {
                if let Some(reason) = error.provider_validation_reason() {
                    reason.retryability()
                } else {
                    ProviderDiagnostic::new(
                        error
                            .provider_validation_child()
                            .expect("single-parent validation frame has a child")
                            .clone(),
                    )
                    .retryability()
                }
            }
            Self::Dispatch(error) => match error {
                crate::provider::CustomTypeDispatchError::Validation(category) => {
                    ProviderDiagnostic::new(category.as_ref().clone()).retryability()
                }
                crate::provider::CustomTypeDispatchError::Undeclared(_)
                | crate::provider::CustomTypeDispatchError::Decode(_) => Retryability::Unclassified,
            },
        }
    }
    fn source(self) -> Option<ProviderCause<'a>> {
        let plain = |error| ProviderCause {
            error,
            secrets: RedactionContext::default(),
        };
        match self {
            Self::Provider(error) => error.detail().cause().map(Diagnostic::as_cause),
            Self::Host(error) => error.detail().cause().map(Diagnostic::as_cause),
            Self::Diagnostic(error) => Some(error.as_cause()),
            Self::Validation(error) => error.diagnostic().map(Diagnostic::as_cause).or_else(|| {
                error
                    .category()
                    .map(|category| plain(category as &(dyn std::error::Error + 'static)))
            }),
            Self::ValidationCategory(error) => error
                .provider_validation_reason()
                .map(|reason| plain(reason as &(dyn std::error::Error + 'static)))
                .or_else(|| {
                    error
                        .provider_validation_child()
                        .map(|child| plain(child as &(dyn std::error::Error + 'static)))
                }),
            Self::Dispatch(error) => {
                use std::error::Error as _;
                error.source().map(plain)
            }
        }
    }
}

mod diagnostic_source_sealed {
    pub trait Sealed<S> {}
}
pub trait DiagnosticSource<S: TextState>:
    Send + Sync + 'static + diagnostic_source_sealed::Sealed<S>
{
    fn into_diagnostic(self) -> Diagnostic<S>;
}
impl<E: std::error::Error + Send + Sync + 'static> diagnostic_source_sealed::Sealed<Unbound> for E {}
impl<E: std::error::Error + Send + Sync + 'static> DiagnosticSource<Unbound> for E {
    fn into_diagnostic(self) -> ProviderDiagnostic {
        ProviderDiagnostic::capture_host_or_provider(self)
    }
}
struct OpaqueProviderFailure(crate::provider::ProviderError);
impl fmt::Debug for OpaqueProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider cause>")
    }
}
impl fmt::Display for OpaqueProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider cause>")
    }
}
// Raw Error walkers must stop here. Only ProviderCause::source may traverse
// the stored diagnostic, carrying its context without exposing a formatter.
impl std::error::Error for OpaqueProviderFailure {}
impl diagnostic_source_sealed::Sealed<Unbound> for crate::provider::ProviderError {}
impl DiagnosticSource<Unbound> for crate::provider::ProviderError {
    fn into_diagnostic(self) -> ProviderDiagnostic {
        let text = self.diagnostic_text();
        let retryability = self.retryability();
        ProviderDiagnostic::captured(
            OpaqueProviderFailure(self),
            ErrorText::from_provider_text(text),
            retryability,
        )
    }
}
impl diagnostic_source_sealed::Sealed<Unbound> for ProviderDiagnostic {}
impl DiagnosticSource<Unbound> for ProviderDiagnostic {
    fn into_diagnostic(self) -> ProviderDiagnostic {
        self
    }
}
macro_rules! host_sources {
    ($($ty:ty),* $(,)?) => { $(
        impl diagnostic_source_sealed::Sealed<Bound> for $ty {}
        impl DiagnosticSource<Bound> for $ty { fn into_diagnostic(self) -> HostDiagnostic { HostDiagnostic::capture_host_or_provider(self) } }
    )* };
}
host_sources!(crate::provider::HostError, InvalidRequiredPermission);
impl diagnostic_source_sealed::Sealed<Bound> for HostDiagnostic {}
impl DiagnosticSource<Bound> for HostDiagnostic {
    fn into_diagnostic(self) -> HostDiagnostic {
        self
    }
}

/// A provider's typed error and its opaque diagnostic.
///
/// The complete source chain is captured at construction. Type-only inspection
/// remains available; textual projections require the call's secret context.
#[derive(Clone)]
pub struct Diagnostic<S: TextState = Unbound> {
    error: std::sync::Arc<dyn std::error::Error + Send + Sync>,
    text: Box<ErrorText<S>>,
    retryability: Retryability,
}

impl<S: TextState> fmt::Debug for Diagnostic<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderDiagnostic")
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

impl<S: TextState> Diagnostic<S> {
    pub(crate) fn text(&self) -> &ErrorText<S> {
        &self.text
    }
    pub fn new(error: impl DiagnosticSource<S>) -> Self {
        error.into_diagnostic()
    }
    pub(crate) fn capture_host_or_provider(
        error: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        let mut node = Some(ProviderCause {
            error: &error,
            secrets: RedactionContext::default(),
        });
        let mut retryability = Retryability::Unclassified;
        while let Some(cause) = node {
            if let Some(frame) = opaque_frame(cause.error) {
                let classification = frame.retryability();
                if classification != Retryability::Unclassified {
                    retryability = classification;
                    break;
                }
            }
            node = cause.source();
        }
        let text = Self::capture(&error);
        Self::captured(error, text, retryability)
    }
    fn captured(
        error: impl std::error::Error + Send + Sync + 'static,
        text: ErrorText<S>,
        retryability: Retryability,
    ) -> Self {
        Self {
            error: std::sync::Arc::new(error),
            text: Box::new(text),
            retryability,
        }
    }
    pub(crate) fn message_for_state(text: ErrorText<S>) -> Self {
        Self::captured(OpaqueMessage, text, Retryability::Unclassified)
    }
    pub fn retryability(&self) -> Retryability {
        self.retryability
    }
    pub(crate) fn capture(error: &(dyn std::error::Error + 'static)) -> ErrorText<S> {
        if let Some(frame) = opaque_frame(error) {
            return frame.text();
        }
        Self::capture_foreign(error)
    }
    fn capture_foreign(error: &(dyn std::error::Error + 'static)) -> ErrorText<S> {
        let mut text = ErrorText::<S>::new(error.to_string());
        if let Some(source) = error.source() {
            text.append_host(": ");
            text.append(&Self::capture(source));
        }
        let context = text.redaction_context();
        text.with_redaction_context(&context)
    }
    pub(crate) fn with_redaction_context(mut self, context: &RedactionContext) -> Self {
        self.text = Box::new(self.text.with_redaction_context(context));
        self
    }
    pub(crate) fn known_secrets(&self) -> Option<RedactionSet> {
        self.text.known_secrets()
    }
    pub(crate) fn has_throttling_marker(&self) -> bool {
        self.text.opaque().has_throttling_marker()
    }
    pub(crate) fn has_delete_dependency_marker(&self) -> bool {
        self.text.opaque().has_delete_dependency_marker()
    }

    /// Type-only inspection cannot reveal a provider cause's contents.
    pub fn is<T: std::error::Error + 'static>(&self) -> bool {
        self.error.is::<T>()
    }
    pub fn as_cause(&self) -> ProviderCause<'_> {
        ProviderCause {
            error: self.error.as_ref(),
            secrets: self.text.redaction_context(),
        }
    }
    #[cfg(test)]
    pub fn error(&self) -> ProviderCause<'_> {
        self.as_cause()
    }
    pub fn source_is<T: std::error::Error + 'static>(&self) -> bool {
        self.as_cause().source_is::<T>()
    }
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<ProviderCause<'_>> {
        self.is::<T>().then(|| ProviderCause {
            error: self.error.as_ref(),
            secrets: self.text.redaction_context(),
        })
    }
    pub fn source(&self) -> Option<ProviderCause<'_>> {
        self.as_cause().source()
    }
    pub(crate) fn normalization_with_context(
        &self,
        secrets: &RedactionSet,
    ) -> Option<RedactedNormalizationCause<'_>> {
        ProviderCause {
            error: self.error.as_ref(),
            secrets: self.text.redaction_context().with(secrets),
        }
        .normalization()
    }
    #[cfg(test)]
    pub(crate) fn project_redacted_with_context<T: std::error::Error + 'static>(
        &self,
        secrets: &RedactionSet,
        project: impl FnOnce(&T) -> String,
    ) -> Option<RedactedProviderText> {
        ProviderCause {
            error: self.error.as_ref(),
            secrets: self.text.redaction_context().with(secrets),
        }
        .project_redacted(project)
    }

    pub(crate) fn opaque(&self) -> ProviderText {
        self.text.opaque()
    }
    pub(crate) fn redacted_str(&self) -> String {
        self.text.redacted_str()
    }
}

impl Diagnostic<Unbound> {
    pub fn message(text: ProviderText) -> Self {
        Self::message_for_state(ErrorText::from_provider_text(text))
    }
    pub(crate) fn with_secrets(self, secrets: &RedactionSet) -> HostDiagnostic {
        Diagnostic {
            error: self.error,
            text: Box::new(self.text.into_host(secrets)),
            retryability: self.retryability,
        }
    }
}
#[cfg(test)]
impl Diagnostic<Unbound> {
    pub fn into_host_for_test(self) -> HostDiagnostic {
        self.with_secrets(&RedactionSet::for_test(&[]))
    }
    pub fn normalization_for_test(&self) -> Option<RedactedNormalizationCause<'_>> {
        self.normalization_with_context(&RedactionSet::for_test(&[]))
    }
    pub fn with_secrets_for_test(self, secrets: &RedactionSet) -> HostDiagnostic {
        self.with_secrets(secrets)
    }
}

impl Diagnostic<Bound> {
    pub(crate) fn host_for_state<S: TextState>(self) -> Diagnostic<S> {
        Diagnostic {
            error: self.error,
            text: Box::new(self.text.host_for_state()),
            retryability: self.retryability,
        }
    }

    /// Structured comparison policy, with textual fields projected through
    /// this diagnostic's retained context. No original typed cause escapes.
    pub fn comparison_resolution(&self) -> Option<crate::provider::ComparisonResolutionError> {
        use crate::provider::ComparisonResolutionError as E;
        use crate::resource::ResourceIdentityState;
        let source = self.error.downcast_ref::<E>()?;
        let secrets = self
            .known_secrets()
            .unwrap_or_else(RedactionSet::before_request);
        let redact = |text: &str| ProviderText::new(text).redact(&secrets).as_str().to_owned();
        let id = |source: &crate::resource::ResourceId| {
            let mut id = source.clone();
            id.provider = redact(&id.provider);
            id.resource_type = redact(&id.resource_type);
            id.provider_instance = id.provider_instance.as_deref().map(redact);
            if let ResourceIdentityState::Resolved(identity) = &id.identity {
                id.identity = ResourceIdentityState::Resolved(
                    crate::resource::ResourceIdentity::try_from(redact(identity.as_str()))
                        .expect("redaction preserves a nonempty identity"),
                );
            }
            id
        };
        Some(match source {
            E::MissingResourceSchema { id: source } => E::MissingResourceSchema { id: id(source) },
            E::ComparisonNotResolvedAtApplyStart { id: source } => {
                E::ComparisonNotResolvedAtApplyStart { id: id(source) }
            }
            E::SchemaOwnerMismatch { id: source, owner } => E::SchemaOwnerMismatch {
                id: id(source),
                owner: redact(owner),
            },
            E::UnknownProviderInstance { instance } => E::UnknownProviderInstance {
                instance: crate::provider::ProviderInstanceKey::new(
                    redact(instance.provider()),
                    instance.provider_instance().map(redact),
                ),
            },
        })
    }

    pub fn normalization(&self) -> Option<RedactedNormalizationCause<'_>> {
        self.normalization_with_context(&RedactionSet::before_request())
    }
    #[cfg(test)]
    pub fn project_redacted<T: std::error::Error + 'static>(
        &self,
        project: impl FnOnce(&T) -> String,
    ) -> Option<RedactedProviderText> {
        self.project_redacted_with_context(&RedactionSet::before_request(), project)
    }
}

#[derive(Debug, thiserror::Error)]
#[error("<provider message>")]
struct OpaqueMessage;
impl<S: TextState> PartialEq for Diagnostic<S> {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}
impl<S: TextState> Serialize for Diagnostic<S> {
    fn serialize<Ser: serde::Serializer>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error> {
        self.text.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Diagnostic<Unbound> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| Self::message(text.into()))
    }
}

/// Type-only view of an original cause. No typed reference or raw formatter
/// can escape; even a downcast returns this opaque view.
///
/// ```compile_fail,E0308
/// use carina_core::provider_text::ProviderCause;
/// fn typed(cause: ProviderCause<'_>) {
///     let _: &std::io::Error = cause.downcast_ref::<std::io::Error>().unwrap();
/// }
/// ```
///
/// ```compile_fail,E0614
/// use carina_core::provider_text::ProviderCause;
/// fn raw<'a>(cause: &'a ProviderCause<'a>) -> &'a dyn std::error::Error {
///     &**cause
/// }
/// ```
///
/// ```compile_fail,E0277
/// fn display(cause: carina_core::provider_text::ProviderCause<'_>) {
///     let _ = format!("{cause}");
/// }
/// ```
///
/// Unbound provider errors have no `Error` implementation. Their private
/// opaque wrapper is traversable, but is not an `is::<ProviderError>` target.
///
/// ```compile_fail,E0277
/// use carina_core::{provider::ProviderError, provider_text::ProviderCause};
/// fn unbound(cause: ProviderCause<'_>) { let _ = cause.is::<ProviderError>(); }
/// ```
#[derive(Clone)]
pub struct ProviderCause<'a> {
    error: &'a (dyn std::error::Error + 'static),
    secrets: RedactionContext,
}
impl fmt::Debug for ProviderCause<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<provider cause>")
    }
}
impl<'a> ProviderCause<'a> {
    pub fn is<T: std::error::Error + 'static>(&self) -> bool {
        self.error.is::<T>()
    }
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<Self> {
        self.is::<T>().then(|| self.clone())
    }
    /// Check a typed cause with a non-capturing test predicate, returning only a bool.
    /// Returns false without calling the predicate when the type does not match.
    ///
    /// ```
    /// use carina_core::provider_text::ProviderDiagnostic;
    /// use std::io::{Error, ErrorKind};
    ///
    /// let diagnostic = ProviderDiagnostic::new(Error::new(ErrorKind::InvalidData, "bad policy"));
    /// let cause = diagnostic.downcast_ref::<Error>().unwrap();
    /// assert!(cause.matches_for_test(|error: &Error| error.kind() == ErrorKind::InvalidData));
    /// ```
    ///
    /// Capturing closures cannot carry values out through this test helper.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::provider_text::ProviderCause;
    /// use std::io::{Error, ErrorKind};
    ///
    /// fn captures(cause: ProviderCause<'_>, expected: ErrorKind) {
    ///     cause.matches_for_test::<Error>(|error| error.kind() == expected);
    /// }
    /// ```
    #[cfg(any(test, feature = "test-utils"))]
    pub fn matches_for_test<T: std::error::Error + 'static>(
        &self,
        predicate: fn(&T) -> bool,
    ) -> bool {
        let mut current = Some(self.clone());
        while let Some(cause) = current {
            if cause.error.downcast_ref::<T>().is_some_and(predicate) {
                return true;
            }
            current = cause.source();
        }
        false
    }
    /// Search all descendants while leaving `is` and `downcast_ref` current-node only.
    pub fn source_is<T: std::error::Error + 'static>(&self) -> bool {
        let mut current = self.source();
        while let Some(cause) = current {
            if cause.is::<T>() {
                return true;
            }
            current = cause.source();
        }
        false
    }
    pub fn source(&self) -> Option<Self> {
        let mut cause = if let Some(frame) = opaque_frame(self.error) {
            frame.source()?
        } else {
            Self {
                error: self.error.source()?,
                secrets: RedactionContext::default(),
            }
        };
        cause.secrets.extend(&self.secrets);
        if let Some(frame) = opaque_frame(cause.error) {
            cause
                .secrets
                .extend(&frame.text::<Unbound>().redaction_context());
        }
        Some(cause)
    }
    #[cfg(test)]
    pub fn redact(&self, _fixture_inventory: &RedactionSet) -> RedactedProviderText {
        let text = ProviderDiagnostic::capture(self.error).opaque();
        text.redact(
            &self
                .secrets
                .snapshot()
                .unwrap_or_else(RedactionSet::before_request),
        )
    }
    /// Controlled projection of a typed cause. Only the redacted result can
    /// leave the projection; this view retains the boundary's inventory.
    #[cfg(test)]
    pub fn project_redacted<T: std::error::Error + 'static>(
        &self,
        project: impl FnOnce(&T) -> String,
    ) -> Option<RedactedProviderText> {
        self.error.downcast_ref::<T>().map(|error| {
            ProviderText::new(project(error)).redact(
                &self
                    .secrets
                    .snapshot()
                    .unwrap_or_else(RedactionSet::before_request),
            )
        })
    }

    pub(crate) fn normalization(&self) -> Option<RedactedNormalizationCause<'a>> {
        use carina_provider_protocol::normalization::{
            NormalizationError, NormalizationErrorKind as Kind, NormalizationPathSegment as Segment,
        };
        let error = self.error.downcast_ref::<NormalizationError>()?;
        let secrets = self
            .secrets
            .snapshot()
            .unwrap_or_else(RedactionSet::before_request);
        let redact = |text: &str| ProviderText::new(text).redact(&secrets).to_string();
        let redact_id = |id: &carina_provider_protocol::types::ResourceId| {
            carina_provider_protocol::types::ResourceId {
                provider: redact(&id.provider),
                resource_type: redact(&id.resource_type),
                identity: redact(&id.identity),
            }
        };
        let kind = match &error.kind {
            Kind::DuplicateKey { key } => Kind::DuplicateKey { key: redact(key) },
            Kind::ResourceIdMismatch { expected, actual } => Kind::ResourceIdMismatch {
                expected: Box::new(redact_id(expected)),
                actual: Box::new(redact_id(actual)),
            },
            kind => kind.clone(),
        };
        Some(RedactedNormalizationCause {
            resource_id: Box::new(redact_id(&error.resource_id)),
            attribute: redact(&error.attribute),
            path: error
                .path
                .iter()
                .map(|segment| match segment {
                    Segment::Key(key) => Segment::Key(redact(key)),
                    Segment::Index(index) => Segment::Index(*index),
                })
                .collect(),
            kind,
            source: self.source(),
        })
    }
}

/// Normalization policy metadata, with every textual field redacted. The
/// protocol's structural kind/path stay useful without exposing the cause.
#[derive(Debug)]
pub struct RedactedNormalizationCause<'a> {
    pub resource_id: Box<carina_provider_protocol::types::ResourceId>,
    pub attribute: String,
    pub path: Vec<carina_provider_protocol::normalization::NormalizationPathSegment>,
    pub kind: carina_provider_protocol::normalization::NormalizationErrorKind,
    source: Option<ProviderCause<'a>>,
}
impl RedactedNormalizationCause<'_> {
    pub fn source(&self) -> Option<ProviderCause<'_>> {
        self.source.clone()
    }
}

impl fmt::Display for Diagnostic<Bound> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.text.fmt(f)
    }
}

impl std::error::Error for Diagnostic<Bound> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // This scalar category has no provider-authored fields. Preserve its
        // typed identity without exposing arbitrary provider causes to Display.
        let mut cause = Some(self.error.as_ref() as &(dyn std::error::Error + 'static));
        while let Some(error) = cause {
            if let Some(category) = error.downcast_ref::<crate::NonFiniteFloatError>() {
                return Some(category);
            }
            cause = error.source();
        }
        None
    }
}

/// Old unit probes compare the absence of display authority with a literal.
/// This test-only adapter exercises that assertion without adding Display.
#[cfg(test)]
pub(crate) trait OpaqueFixtureDisplay {
    fn to_string(&self) -> String {
        "<provider text>".to_owned()
    }
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}
#[cfg(test)]
impl OpaqueFixtureDisplay for crate::provider::ProviderError {}
#[cfg(test)]
impl OpaqueFixtureDisplay for ProviderDiagnostic {}

#[cfg(test)]
mod tests {
    use super::*;
    struct ProviderTextBuilder {
        stream: RedactionStream,
        output: DiagnosticTextBuilder,
    }
    impl ProviderTextBuilder {
        fn new(secrets: RedactionSet) -> Self {
            Self {
                stream: RedactionStream::new(secrets),
                output: DiagnosticTextBuilder::default(),
            }
        }
        fn push_str(&mut self, text: &str) {
            self.stream
                .push_str(text, &mut |chunk| self.output.push_str(chunk));
        }
        fn finish(mut self) -> DiagnosticText {
            self.stream.flush(&mut |chunk| self.output.push_str(chunk));
            self.output.finish()
        }
    }

    fn provider_secrets(values: &[&str]) -> RedactionSet {
        use crate::resource::{ConcreteValue, DeferredValue, Value};
        let attributes: indexmap::IndexMap<String, Value> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                (
                    index.to_string(),
                    Value::Deferred(DeferredValue::Secret(
                        Box::new(Value::Concrete(ConcreteValue::String((*value).into()))).into(),
                    )),
                )
            })
            .collect();
        RedactionSet::collect(attributes.values())
    }

    #[test]
    fn validation_secret_limit_counts_values_instead_of_encoded_spellings() {
        let inputs = provider_secrets(&["single/+秘密"]);
        assert!(inputs.snapshot().len() > 1);
        assert!(!inputs.would_exceed_secret_limit(&provider_secrets(&["single/+秘密"]), 1));
        assert!(inputs.would_exceed_secret_limit(&provider_secrets(&["second-private"]), 1));
    }

    #[test]
    fn multiline_secret_does_not_delay_unrelated_stderr_lines() {
        let secrets = provider_secrets(&[
            "-----BEGIN PRIVATE KEY-----\nprivate-key-body\n-----END PRIVATE KEY-----",
        ]);
        let mut writer = RedactingLineWriter::new(&secrets, Vec::new());
        writer.push_str("public=ready\n").unwrap();
        assert_eq!(
            writer.sink, b"public=ready\n",
            "a complete unrelated line must be visible before teardown"
        );
        writer
            .push_str("-----BEGIN PRIVATE KEY-----\nprivate-")
            .unwrap();
        assert_eq!(
            writer.sink, b"public=ready\n",
            "retain the possible secret prefix"
        );
        writer
            .push_str("key-body\n-----END PRIVATE KEY-----\n")
            .unwrap();
        assert_eq!(writer.sink, b"public=ready\n(secret)\n");
        writer.finish().unwrap();
        assert_eq!(writer.sink, b"public=ready\n(secret)\n");
    }

    #[test]
    fn round4_host_segments_preserve_short_secret_digits() {
        let secrets = provider_secrets(&["4"]);
        let provider = crate::provider::ProviderError::api_error("provider status: 404")
            .with_secrets(&secrets);
        let host = crate::provider::HostError::internal_caused_by("host status: 404", provider);
        assert_eq!(
            host.to_string(),
            "host status: 404: provider status: (secret)0(secret)"
        );
    }

    #[test]
    fn round4_missing_attribute_names_are_structural() {
        let secrets = provider_secrets(&["1", "true"]);
        let diagnostic = crate::provider::PartialReadDiagnostic::new(
            crate::provider_text::ProviderDiagnostic::message("public reason".into()),
            (vec!["subnet_1_id".into()])
                .try_into()
                .expect("nonempty partial attributes"),
        )
        .with_secrets(&secrets);
        assert_eq!(diagnostic.missing_attributes()[0].as_str(), "subnet_1_id");
    }

    #[test]
    fn review_b2_binding_and_builders_defer_redaction_until_projection() {
        let secrets = provider_secrets(&["private"]);
        REDACTION_PASSES.with(|count| count.set(0));
        let error = crate::provider::ProviderError::api_error("private")
            .with_secrets(&secrets)
            .with_code("private")
            .with_request_id("private")
            .for_provider("private");
        assert_eq!(
            REDACTION_PASSES.with(std::cell::Cell::get),
            0,
            "binding/builders must retain source and context without redacting"
        );
        assert!(error.to_string().contains("(secret)"));
        let after_projection = REDACTION_PASSES.with(std::cell::Cell::get);
        let _ = error.to_string();
        assert_eq!(
            REDACTION_PASSES.with(std::cell::Cell::get),
            after_projection,
            "repeat projections must reuse the bound text"
        );
    }

    #[test]
    fn review_b1_scalar_secret_does_not_drop_structured_wire_status() {
        let secrets = RedactionSet::collect([
            &crate::resource::Value::<crate::resource::NoPlaceholder>::Concrete(
                crate::resource::ConcreteValue::Int(3),
            )
            .into_secret(),
            &crate::resource::Value::<crate::resource::NoPlaceholder>::Concrete(
                crate::resource::ConcreteValue::Bool(true),
            )
            .into_secret(),
        ]);
        let host = crate::provider::ProviderError::api_error("password 3 true")
            .with_status(403)
            .with_secrets(&secrets);
        assert_eq!(host.to_wire().status, Some(403));
        assert_eq!(
            host.to_string(),
            "password (secret) (secret)\n  status: 403"
        );
    }

    #[test]
    fn provider_text_host_metadata_does_not_rescan_inserted_masks() {
        use crate::provider::ProviderError;
        let secrets = provider_secrets(&["secret", "3"]);
        let error = ProviderError::api_error("secret")
            .with_status(403)
            .for_provider("secret")
            .with_secrets(&secrets)
            .with_operation("create")
            .with_code("public");
        assert_eq!(
            error.to_string(),
            "(secret)\n  operation: create\n  status: 403 public"
        );
        assert_eq!(
            error
                .detail()
                .provider_name
                .as_ref()
                .map(|text| text.as_diagnostic())
                .as_deref(),
            Some("(secret)")
        );
        assert_eq!(error.to_wire().status, Some(403));
    }

    #[test]
    fn provider_text_host_error_needs_no_secret_binding_and_can_be_relayed() {
        use crate::provider::{HostError, ProviderError};
        let host =
            HostError::internal_caused_by("host context", HostError::internal("host reason"));
        assert_eq!(host.to_string(), "host context: host reason");
        let relayed: ProviderError = host.into();
        assert_eq!(relayed.to_string(), "<provider text>");
        let received = relayed.with_secrets(&provider_secrets(&["host"]));
        assert_eq!(received.to_string(), "(secret) context: (secret) reason");

        let secret_error =
            ProviderError::api_error("secret").with_secrets(&provider_secrets(&["secret"]));
        let host = HostError::internal_caused_by("public context", secret_error);
        assert_eq!(host.to_string(), "public context: (secret)");
        let relayed: ProviderError = host.into();
        let received = relayed.with_secrets(&provider_secrets(&["secret"]));
        assert_eq!(received.to_string(), "public context: (secret)");
    }

    #[test]
    fn provider_text_masks_crossing_overlaps_and_adjacent_short_secrets() {
        let secrets = provider_secrets(&["", "ab", "abcde", "zab"]);
        let redacted = ProviderText::new("zabcde abab public").redact(&secrets);
        assert_eq!(redacted.as_str(), "(secret) (secret)(secret) public");
        let control = "public\r\n";
        assert_eq!(
            ProviderText::new(control).redact(&secrets).as_str(),
            control
        );
    }

    #[test]
    fn provider_text_masks_json_dsl_and_terminal_spellings() {
        let secret = "a\"b\\c\r\t\0${value}";
        let secrets = provider_secrets(&[secret]);
        for spelling in [
            secret.to_string(),
            serde_json::to_string(secret).unwrap(),
            crate::parser::display_string_literal(secret),
            DiagnosticText::new(secret).to_string(),
        ] {
            let redacted = ProviderText::new(format!("public {spelling} context")).redact(&secrets);
            assert!(redacted.contains("(secret)"), "{redacted}");
            assert!(!redacted.contains("a\\\"b"), "{redacted}");
            assert!(redacted.starts_with("public ") && redacted.ends_with(" context"));
        }
    }

    #[test]
    fn provider_text_stream_redacts_before_truncation_and_across_writes() {
        let secrets = provider_secrets(&["ab", "abcde", "zab", "private-tail"]);
        for raw in [
            "zabcde abab".to_string(),
            format!("{}private-tail", "x".repeat(MAX_PROVIDER_TEXT_BYTES - 2)),
        ] {
            let expected = ProviderText::new(&raw).redact(&secrets).to_string();
            let mut stream = ProviderTextBuilder::new(secrets.clone());
            for ch in raw.chars() {
                stream.push_str(&ch.to_string());
            }
            let actual = stream.finish().to_string();
            assert_eq!(actual, expected);
            assert!(!actual.contains("private-"));
        }
    }

    #[test]
    fn provider_error_wire_ingestion_cannot_export_original_diagnostic_text() {
        use crate::provider::ProviderError;
        let secrets = provider_secrets(&["private-password", "DependencyViolation"]);
        let provider = ProviderError::api_error_caused_by(
            "public context",
            std::io::Error::other("private-password"),
        )
        .with_operation("private-password")
        .with_code("DependencyViolation")
        .with_request_id("private-password")
        .for_resource(crate::resource::ResourceId::with_identity(
            "test",
            "private-password",
        ));
        let provider = provider.with_secrets(&RedactionSet::for_test(&[]));
        let wire = provider.to_wire();
        assert!(wire.cause.as_ref().unwrap().contains("private-password"));
        let opaque = ProviderError::from_wire(wire);
        assert_eq!(opaque.to_string(), "<provider text>");
        assert!(!format!("{opaque:?}").contains("private-password"));
        assert!(
            !serde_json::to_string(&opaque.to_wire())
                .unwrap()
                .contains("private-password")
        );
        assert!(opaque.is_delete_dependency());
        let host = opaque.with_secrets(&secrets);
        assert!(!host.to_string().contains("private-password"));
        assert!(host.to_string().contains("public context"));
        assert!(host.is_delete_dependency());
        let encoded = serde_json::to_string(&host.to_wire()).unwrap();
        assert!(!encoded.contains("private-password"));
        assert!(!encoded.contains("DependencyViolation"));
        assert!(encoded.contains("(secret)"));
        let coded = ProviderError::api_error("public")
            .with_code("DependencyViolation")
            .with_secrets(&secrets);
        assert!(coded.is_delete_dependency());
        assert!(!coded.with_code("other").is_delete_dependency());
    }

    #[test]
    fn provider_error_wrapping_retains_secret_context_and_host_ingestion() {
        use crate::provider::ProviderError;
        let secrets = provider_secrets(&["configuration-password"]);
        let inner = ProviderError::api_error("configuration-password").with_secrets(&secrets);
        let outer = ProviderError::internal_caused_by("public context", Box::new(inner))
            .with_secrets(&secrets);
        assert_eq!(outer.to_string(), "public context: (secret)");
        assert!(
            !serde_json::to_string(&outer.to_wire())
                .unwrap()
                .contains("configuration-password")
        );
        let cause = outer.detail().cause().unwrap();
        assert_eq!(cause.redacted_str(), "(secret)");
        assert_eq!(
            cause
                .downcast_ref::<Box<crate::provider::HostError>>()
                .unwrap()
                .redact(&RedactionSet::for_test(&[]))
                .as_str(),
            "(secret)"
        );

        let incoming = ProviderError::from_wire(
            ProviderError::api_error("configuration-password")
                .with_secrets(&RedactionSet::for_test(&[]))
                .to_wire(),
        );
        let wrapper = ProviderError::internal_caused_by("public context", incoming);
        assert_eq!(wrapper.to_string(), "<provider text>");
        assert!(
            !serde_json::to_string(&wrapper.to_wire())
                .unwrap()
                .contains("configuration-password")
        );
        assert_eq!(
            wrapper.with_secrets(&secrets).to_string(),
            "public context: (secret)"
        );
    }

    #[test]
    fn provider_error_redacted_wire_preserves_nonsecret_text_without_display_truncation() {
        use crate::provider::ProviderError;
        let message = format!("{}\r\npublic tail", "x".repeat(MAX_PROVIDER_TEXT_BYTES + 1));
        let cause = format!(
            "{} DependencyViolation",
            "y".repeat(MAX_PROVIDER_TEXT_BYTES + 1)
        );
        let error = ProviderError::api_error_caused_by(
            "public context",
            std::io::Error::other(cause.clone()),
        )
        .with_code(message.clone())
        .with_secrets(&RedactionSet::for_test(&[]));
        let wire = error.to_wire();
        assert_eq!(wire.code.as_deref(), Some(message.as_str()));
        assert_eq!(wire.cause.as_deref(), Some(cause.as_str()));
        assert!(ProviderError::from_wire(wire).is_delete_dependency());
    }

    #[test]
    fn diagnostic_renders_context_and_typed_cause_once() {
        #[derive(Debug)]
        struct Context(crate::NonFiniteFloatError);
        impl fmt::Display for Context {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("invalid ratio")
            }
        }
        impl std::error::Error for Context {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let diagnostic = ProviderDiagnostic::new(Context(crate::NonFiniteFloatError::NaN))
            .with_secrets(&RedactionSet::for_test(&[]));
        assert_eq!(
            diagnostic.to_string(),
            "invalid ratio: non-finite float NaN"
        );
        assert!(
            diagnostic
                .error()
                .source()
                .unwrap()
                .is::<crate::NonFiniteFloatError>()
        );
    }

    #[test]
    fn nested_provider_error_cause_is_reachable_unbound() {
        use crate::provider::ProviderError;

        let inner = ProviderError::api_error_caused_by(
            "inner context",
            std::io::Error::other("private cause"),
        );
        let outer = ProviderError::api_error_caused_by("read error", inner);
        let diagnostic = outer.detail().cause().unwrap();
        let opaque = diagnostic.as_cause();
        assert_eq!(format!("{opaque:?}"), "<provider cause>");
        assert!(!opaque.is::<std::io::Error>());
        let source = opaque.source().expect("nested I/O cause must be reachable");
        assert!(source.is::<std::io::Error>());
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(format!("{source:?}"), "<provider cause>");
        assert!(source.source().is_none());
        assert!(diagnostic.source_is::<std::io::Error>());
        assert!(diagnostic.source().unwrap().is::<std::io::Error>());
        assert!(!format!("{outer:?} {diagnostic:?}").contains("private cause"));
    }

    #[test]
    fn nested_provider_error_cause_retains_inner_and_outer_redaction_contexts() {
        use crate::provider::ProviderError;

        let inner_secrets = provider_secrets(&["inner-password"]);
        let inner: ProviderError = ProviderError::api_error_caused_by(
            "inner context",
            std::io::Error::other("inner-password outer-password"),
        )
        .with_secrets(&inner_secrets)
        .into();
        let middle = ProviderError::api_error_caused_by("middle context", inner);
        let outer = ProviderError::api_error_caused_by("read error", middle);
        let empty = RedactionSet::for_test(&[]);
        let diagnostic = outer.detail().cause().unwrap();
        let nested = diagnostic
            .as_cause()
            .source()
            .expect("nested provider cause");
        let source = nested.source().expect("nested I/O cause");
        assert!(source.is::<std::io::Error>());
        assert_eq!(source.redact(&empty).as_str(), "(secret) outer-password");

        let outer_secrets = provider_secrets(&["outer-password"]);
        let host = outer.with_secrets(&outer_secrets);
        let diagnostic = host.detail().cause().unwrap();
        let source = diagnostic.source().unwrap().source().unwrap();
        assert!(source.is::<std::io::Error>());
        assert_eq!(source.redact(&empty).as_str(), "(secret) (secret)");
        let rendered = format!(
            "{host} {host:?} {diagnostic} {diagnostic:?} {source:?} {}",
            serde_json::to_string(&host.to_wire()).unwrap(),
        );
        assert!(rendered.contains("inner context"));
        for secret in ["inner-password", "outer-password"] {
            assert!(!rendered.contains(secret), "secret escaped a nested cause");
        }
    }

    #[test]
    fn nested_provider_error_raw_source_walk_stays_opaque() {
        use crate::provider::ProviderError;

        let secrets = provider_secrets(&["private cause"]);
        let inner = ProviderError::api_error_caused_by(
            "inner context",
            std::io::Error::other("private cause"),
        );
        let diagnostic = ProviderDiagnostic::new(inner).with_secrets(&secrets);
        let stored = diagnostic.error.as_ref();
        assert_eq!(stored.to_string(), "<provider cause>");
        assert!(!format!("{stored:?}").contains("private cause"));
        assert!(stored.source().is_none());
        assert!(std::error::Error::source(&diagnostic).is_none());
        assert!(diagnostic.source_is::<std::io::Error>());

        let inner =
            ProviderError::api_error_caused_by("inner context", crate::NonFiniteFloatError::NaN);
        let diagnostic = ProviderDiagnostic::new(inner).with_secrets(&secrets);
        assert!(diagnostic.source_is::<crate::NonFiniteFloatError>());
        assert!(std::error::Error::source(&diagnostic).is_none());
    }

    #[test]
    fn provider_text_builder_bounds_storage_and_counts_the_entire_tail() {
        let mut builder = DiagnosticTextBuilder::default();
        for _ in 0..32 {
            builder.push_str(&"x".repeat(1024));
            assert!(builder.prefix.len() <= MAX_PROVIDER_TEXT_BYTES);
            assert!(builder.prefix.capacity() <= MAX_PROVIDER_TEXT_BYTES);
        }
        builder.push_str("\x1b[2J");
        assert!(builder.prefix.len() <= MAX_PROVIDER_TEXT_BYTES);
        let text = builder.finish();
        assert!(text.len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(!text.contains('\x1b'));
        let (prefix, marker) = text.split_once("… (truncated, ").unwrap();
        let omitted: usize = marker.strip_suffix(" bytes)").unwrap().parse().unwrap();
        assert_eq!(prefix.len() + omitted, 32 * 1024 + "\\u{1b}[2J".len());
    }

    #[test]
    fn provider_text_escapes_terminal_sequences() {
        let text = DiagnosticText::new("\x1b]52;c;ZXZpbA==\x07 \x1b[2J");
        assert_eq!(text.as_str(), "\\u{1b}]52;c;ZXZpbA==\\u{7} \\u{1b}[2J");
        assert!(!text.to_string().contains(['\x1b', '\x07']));
    }

    #[test]
    fn provider_text_neutralizes_sgr_cursor_and_osc_sequences() {
        for raw in [
            "\x1b[31mred\x1b[0m",
            "\x1b[5A",
            "\x1b[H",
            "\x1b]0;evil\x07",
            "\x1b]0;evil\x1b\\",
        ] {
            let text = DiagnosticText::new(raw);
            assert!(!text.as_bytes().contains(&0x1b));
            assert!(!text.as_bytes().contains(&0x07));
            assert!(text.contains("\\u{1b}"), "{text}");
        }
    }

    #[test]
    fn provider_text_utf8_boundary_drops_three_and_four_byte_chars_whole() {
        // Exercise both the initial byte limit and the cut after reserving
        // space for the truncation marker.
        let marker_size = "… (truncated, 12317 bytes)".len();
        for c in ['あ', '🦀'] {
            for leading in [
                MAX_PROVIDER_TEXT_BYTES - 1,
                MAX_PROVIDER_TEXT_BYTES - marker_size - 1,
            ] {
                let raw = format!(
                    "{}{c}{}",
                    "x".repeat(leading),
                    "y".repeat(20 * 1024 - leading - c.len_utf8())
                );
                let expected_prefix =
                    "x".repeat(leading.min(MAX_PROVIDER_TEXT_BYTES - marker_size));
                let omitted = raw.len() - expected_prefix.len();
                let text = DiagnosticText::new(raw);
                assert!(std::str::from_utf8(text.as_bytes()).is_ok());
                assert!(!text.contains(c));
                assert!(text.len() <= MAX_PROVIDER_TEXT_BYTES);
                assert_eq!(
                    text.as_str(),
                    format!("{expected_prefix}… (truncated, {omitted} bytes)")
                );
            }
        }
    }

    #[test]
    fn provider_text_preserves_newlines_and_tabs_but_escapes_other_controls() {
        let text = DiagnosticText::new("line\n\tindent\r\x7f\u{009b}\0");
        assert_eq!(text.as_str(), "line\n\tindent\\u{d}\\u{7f}\\u{9b}\\u{0}");
        for control in ('\0'..='\u{9f}').filter(|c| c.is_control()) {
            let text = DiagnosticText::new(control.to_string());
            let expected = match control {
                '\n' | '\t' => control.to_string(),
                c => c.escape_unicode().to_string(),
            };
            assert_eq!(text.as_str(), expected);
        }
    }

    #[test]
    fn provider_text_caps_twenty_kib_with_exact_omitted_byte_count() {
        let text = DiagnosticText::new("x".repeat(20 * 1024));
        let marker = "… (truncated, 12316 bytes)";
        assert_eq!(text.as_str(), format!("{}{marker}", "x".repeat(8164)));
        assert_eq!(text.as_str().len(), MAX_PROVIDER_TEXT_BYTES);
    }

    #[test]
    fn provider_text_truncates_sanitized_utf8_at_a_character_boundary() {
        let raw = format!("{}{}", "\x1b".repeat(1360), "界".repeat(5000));
        let sanitized = format!("{}{}", "\\u{1b}".repeat(1360), "界".repeat(5000));
        let text = DiagnosticText::new(raw);
        let (prefix, marker) = text.as_str().split_once("… (truncated, ").unwrap();
        assert!(text.as_str().len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(sanitized.starts_with(prefix));
        assert!(prefix.ends_with('界'));
        assert_eq!(marker, format!("{} bytes)", sanitized.len() - prefix.len()));
        assert!(std::str::from_utf8(text.as_str().as_bytes()).is_ok());
    }

    #[test]
    fn provider_text_caps_twenty_kib_on_a_multibyte_boundary() {
        let raw = format!("{}é", "界".repeat(6826));
        assert_eq!(raw.len(), 20 * 1024);
        let text = DiagnosticText::new(raw);
        assert_eq!(
            text.as_str(),
            format!("{}… (truncated, 12317 bytes)", "界".repeat(2721))
        );
        assert_eq!(text.as_str().len(), MAX_PROVIDER_TEXT_BYTES - 1);
    }

    #[test]
    fn provider_text_leaves_text_at_the_limit_unchanged() {
        use super::DiagnosticText as ProviderText;
        let raw = "x".repeat(MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(ProviderText::new(&raw).as_str(), raw);
    }

    #[test]
    fn provider_text_all_construction_paths_sanitize() {
        let raw = "\x1b[2J";
        let json = serde_json::to_string(raw).unwrap();
        let decoded: DiagnosticText = serde_json::from_str(&json).unwrap();
        for text in [
            DiagnosticText::from(raw),
            DiagnosticText::from(raw.to_string()),
            decoded,
        ] {
            assert_eq!(text.as_str(), "\\u{1b}[2J");
            assert_eq!(serde_json::to_string(&text).unwrap(), r#""\\u{1b}[2J""#);
        }
    }

    #[test]
    fn provider_text_native_errors_sanitize_messages_metadata_and_typed_causes() {
        use crate::provider::ProviderError;
        let raw = "\x1b]52;c;ZXZpbA==\x07\x1b[2J";
        for error in [
            ProviderError::invalid_input(raw)
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            ProviderError::api_error(raw)
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            ProviderError::not_found(raw)
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            ProviderError::timeout(raw)
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            ProviderError::internal(raw)
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
            ProviderError::internal_caused_by("native error", std::io::Error::other(raw))
                .with_secrets(&crate::provider_text::RedactionSet::for_test(&[])),
        ] {
            let error = error
                .for_provider(raw)
                .with_operation(raw)
                .with_code(raw)
                .with_request_id(raw);
            assert!(!error.to_string().contains(['\x1b', '\x07']));
            assert_eq!(
                error
                    .detail()
                    .provider_name
                    .as_ref()
                    .map(|text| text.as_diagnostic())
                    .as_deref(),
                Some("\\u{1b}]52;c;ZXZpbA==\\u{7}\\u{1b}[2J")
            );
            if let Some(cause) = error.detail().cause() {
                assert!(cause.downcast_ref::<std::io::Error>().is_some());
            }
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn provider_text_semantic_strings_keep_raw_equality_and_serialization() {
        use super::ProviderKey as ProviderString;
        let raw = format!("key\x1b[2J{}", "x".repeat(9000));
        let data = ProviderString::from(raw.clone());
        let label = data.to_string();
        assert!(!label.contains('\x1b'));
        assert!(label.len() <= MAX_PROVIDER_TEXT_BYTES);
        assert!(label.contains("\\u{1b}[2J"));
        assert_ne!(data, ProviderString::from(label));
        assert_eq!(data.as_str(), raw);
        let encoded = serde_json::to_string(&data).unwrap();
        assert_eq!(serde_json::from_str::<String>(&encoded).unwrap(), raw);
        let decoded: ProviderString = serde_json::from_str(&encoded).unwrap();
        assert_eq!(data, decoded);
        assert_eq!(decoded.into_string(), raw);
    }

    #[test]
    fn provider_text_partial_read_preserves_raw_keys_for_lookup_and_writeback() {
        use crate::binding_index::{AppliedOperation, PartialReadBack};
        use crate::provider::{PartialReadDiagnostic, ReadBackEnd};
        use crate::resource::{ResourceId, State};

        for raw in ["computed\x1b[2J".to_string(), "x".repeat(9000)] {
            let diagnostic = PartialReadDiagnostic::new(
                "incomplete".into(),
                (vec![raw.clone()])
                    .try_into()
                    .expect("nonempty partial attributes"),
            );
            let read_back = PartialReadBack::from_diagnostic(
                AppliedOperation::Create,
                &diagnostic,
                ReadBackEnd::ProviderReported,
            );
            assert!(read_back.attribute_is_missing(&raw));
            let mut state = diagnostic.into_state_for_writeback(State::existing(
                ResourceId::with_identity("test", "example"),
                Default::default(),
            ));
            assert!(
                state
                    .partial_read
                    .as_ref()
                    .unwrap()
                    .missing_attributes()
                    .contains(&raw)
            );
            state.materialize_partial_read_unknowns();
            assert!(state.attributes.contains_key(&raw));
        }
    }

    #[test]
    fn provider_text_error_resource_identity_stays_raw() {
        use crate::provider::ProviderError;
        use crate::resource::ResourceId;
        let raw = "example\x1b[2J";
        let id = ResourceId::with_identity("test", raw);
        let error = ProviderError::internal("failed")
            .with_secrets(&crate::provider_text::RedactionSet::for_test(&[]))
            .for_resource(id.clone());
        assert_eq!(
            error
                .detail()
                .resource_id
                .as_ref()
                .unwrap()
                .as_resource_id(),
            &id
        );
        assert!(error.to_string().contains("example\\u{1b}[2J"));
        assert!(!error.to_string().contains('\x1b'));
        assert_eq!(error.to_wire().resource_id.unwrap().identity, raw);
    }

    #[test]
    fn provider_text_enum_metadata_stays_raw_and_displays_safely() {
        use crate::schema::{
            CompiledAttributeType, DiagnosticValue, ExpectedEnumVariant, InvalidEnumValueKind,
            TypeError, TypeIdentity,
        };
        let raw = "bad\x1b[2J\x07";
        let expected = ExpectedEnumVariant::from_namespaced(Some(raw), raw, raw, false);
        assert_eq!(expected.provider.as_deref(), Some(raw));
        assert_eq!(expected.value.as_str(), raw);
        assert!(!expected.to_string().contains(['\x1b', '\x07']));
        assert!(expected.to_string().contains("\\u{1b}[2J\\u{7}"));
        let schema = CompiledAttributeType::enum_(
            TypeIdentity::new(None::<&str>, Vec::<String>::new(), raw),
            Some(vec![raw.into()]),
            vec![],
            None,
            None,
        );
        let value: crate::resource::Value =
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::enum_identifier(raw));
        assert!(schema.validate(&value).is_ok());
        let invalid: crate::resource::Value = crate::resource::Value::Concrete(
            crate::resource::ConcreteValue::enum_identifier("missing"),
        );
        assert!(
            !schema
                .validate(&invalid)
                .unwrap_err()
                .to_string()
                .contains(['\x1b', '\x07'])
        );
        for error in [
            TypeError::InvalidEnumVariant {
                value: DiagnosticValue::from("missing"),
                value_kind: InvalidEnumValueKind::Identifier,
                attribute: Some(raw.into()),
                type_name: Some(raw.into()),
                expected: vec![expected],
            },
            TypeError::value_type_mismatch(raw.into(), "Int".into()),
            TypeError::MissingRequired { name: raw.into() },
            TypeError::UnknownAttribute {
                name: "unknown".into(),
                suggestion: Some(raw.into()),
            },
            TypeError::UnknownStructField {
                struct_name: raw.into(),
                field: "unknown".into(),
                suggestion: Some(raw.into()),
            },
        ] {
            let rendered = error.to_string();
            assert!(!rendered.contains(['\x1b', '\x07']), "{rendered:?}");
            assert!(rendered.contains("\\u{1b}[2J\\u{7}"));
        }
    }

    #[test]
    fn provider_text_schema_locations_preserve_raw_attribute_paths() {
        use crate::schema::SchemaValidationIssueLocation;
        let raw = "name\x1b[2J";
        let location = SchemaValidationIssueLocation::Attribute(raw.into());
        assert_eq!(location.attribute_path(), Some(raw));
        assert_eq!(location.to_string(), "name\\u{1b}[2J");
    }

    #[test]
    fn provider_text_partial_read_deserialization_cannot_bypass_sanitizing() {
        use crate::provider::PartialReadDiagnostic;
        use crate::resource::{PendingReRead, ReReadReason};
        let raw = "\x1b[2J\x07";
        let json = serde_json::json!({"reason": raw, "missing_attributes": [raw]});
        let decoded: crate::provider::HostPartialReadDiagnostic =
            serde_json::from_value::<PartialReadDiagnostic>(json)
                .unwrap()
                .with_secrets(&RedactionSet::for_test(&[]));
        assert_eq!(decoded.reason(), "\\u{1b}[2J\\u{7}");
        assert_eq!(decoded.raw_reason(), raw);
        assert_eq!(decoded.missing_attributes()[0].as_str(), raw);
        assert_eq!(
            decoded.missing_attributes()[0].to_string(),
            "\\u{1b}[2J\\u{7}"
        );
        let decoded: PendingReRead = serde_json::from_value(serde_json::json!({
            "reason": {"kind": "incomplete", "detail": raw}
        }))
        .unwrap();
        let ReReadReason::Incomplete { detail } = decoded.reason() else {
            panic!("expected an incomplete re-read reason");
        };
        assert_eq!(detail.as_str(), raw);
        let rendered = detail.to_string();
        assert_eq!(rendered, "\\u{1b}[2J\\u{7}");
    }

    #[test]
    fn provider_text_pattern_mismatch_keeps_regex_data_and_sanitizes_diagnostic() {
        use crate::schema::{CompiledAttributeType, CompiledPattern, TypeError};
        let raw = format!("^\x1b{}$", "x".repeat(20 * 1024));
        let pattern = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            &raw,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap();
        let schema = CompiledAttributeType::refined_string(None, Some(pattern.clone()), None, None);
        let value: crate::resource::Value =
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::String("safe".into()));
        let error = schema.validate(&value).unwrap_err();
        assert!(!error.to_string().contains('\x1b'));
        let TypeError::PatternMismatch {
            pattern: diagnostic,
            ..
        } = error
        else {
            panic!("expected a pattern mismatch");
        };
        assert!(diagnostic.to_string().len() <= MAX_PROVIDER_TEXT_BYTES);
        assert_eq!(diagnostic.as_str(), raw);
        assert_eq!(pattern.as_str(), raw);
    }

    #[test]
    fn provider_text_invalid_schema_metadata_stays_raw_and_displays_safely() {
        use crate::protocol_convert::SchemaDecodeError;
        use crate::schema::{CompiledPattern, ProviderSchemaValidationError, TypeIdentity};
        let raw = format!("name\x1b[2J{}", "x".repeat(9000));
        let invalid_pattern = format!("{raw}(");
        let error = CompiledPattern::try_new(&crate::schema::PatternSpec::new(
            &invalid_pattern,
            crate::schema::PatternMatch::Full,
        ))
        .unwrap_err();
        assert_eq!(error.as_ref().as_str(), invalid_pattern);
        assert!(!error.to_string().contains('\x1b'));
        let identity = TypeIdentity::new(Some("mock"), [raw.as_str()], "Mode");
        let error = ProviderSchemaValidationError::DanglingReferenceForm {
            identity: identity.clone(),
        };
        assert!(
            matches!(&error, ProviderSchemaValidationError::DanglingReferenceForm { identity: stored } if stored == &identity)
        );
        assert!(!error.to_string().contains('\x1b'));
        let error = SchemaDecodeError::DuplicateReferenceForm { identity };
        assert!(!error.to_string().contains('\x1b'));
        assert!(error.to_string().contains("\\u{1b}[2J"));
        let error = crate::parser::CustomTypeValidationError::ConflictingEnumSpelling {
            identity: Box::new(TypeIdentity::new(None::<&str>, Vec::<String>::new(), &raw)),
            first_owner: "mock".into(),
            first_spelling: raw.clone(),
            second_owner: "mock2".into(),
            second_spelling: "other".into(),
        };
        assert!(!error.to_string().contains('\x1b'));
        assert!(
            matches!(error, crate::parser::CustomTypeValidationError::ConflictingEnumSpelling { first_spelling, .. } if first_spelling == raw)
        );
    }

    #[test]
    fn provider_text_schema_descriptions_and_validation_messages_are_sanitized() {
        use crate::schema::{CompletionValue, PatternResourceSchema, TypeError, legacy_validator};
        let raw = format!("\x1b[2J\x07{}", "x".repeat(20 * 1024));
        let schema: PatternResourceSchema =
            PatternResourceSchema::new("test.Resource").with_description(&raw);
        let description = schema.parts().description.unwrap();
        assert!(!description.contains(['\x1b', '\x07']));
        assert!(description.len() <= MAX_PROVIDER_TEXT_BYTES);
        let completion: CompletionValue = serde_json::from_value(serde_json::json!({
            "value": "data", "description": raw
        }))
        .unwrap();
        assert_eq!(completion.value, "data");
        assert!(!completion.description.contains(['\x1b', '\x07']));
        let validator = legacy_validator(move |_| Err((raw.clone()).into()));
        let error = validator(&crate::resource::Value::Concrete(
            crate::resource::ConcreteValue::Bool(true),
        ))
        .unwrap_err();
        let TypeError::ValidationFailed { message } = error else {
            panic!("expected a validation failure");
        };
        assert!(!message.to_string().contains(['\x1b', '\x07']));
        assert!(message.to_string().len() <= MAX_PROVIDER_TEXT_BYTES);
    }
}
