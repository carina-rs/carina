//! Typed provider emission and the private binding kernel shared with returned errors.
use super::*;
use crate::provider::{HostError, ProviderError};
use crate::resource::{ConcreteValue, ResourceId, Value};
use crate::schema::{EffectiveType, SchemaKind, SchemaRegistry};
use carina_provider_protocol::diagnostics as wire;
use carina_provider_protocol::types as protocol;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub use wire::{Level as EventLevel, ValueOrigin};

const MAX_EVENT_INPUT_BYTES: usize = 64 * 1024;
// Panic context, message, and location share a finite 256 KiB input allowance.
// Over-cap panic text is masked as a whole before binding or redaction work.
#[cfg(feature = "guest-runtime")]
const MAX_PANIC_INPUT_BYTES: usize = 256 * 1024;
const MAX_EVENTS: usize = 256;
const MAX_EVENTS_PER_SECOND: usize = 64;
const MAX_EVENT_NODES: usize = 1024;
const MAX_EVENT_DEPTH: usize = 32;
const MAX_REGISTERED_BYTES: usize = 256 * 1024;
const MAX_REGISTERED_VALUES: usize = 1024;

/// Authoring accepts only borrowed diagnostics or structured value candidates.
/// It grants no formatting or binding authority.
pub struct ProviderEvent<'a> {
    severity: EventLevel,
    context: Option<ProviderContext>,
    body: EventInput<'a>,
}
enum EventInput<'a> {
    Failure(&'a ProviderError),
    Diagnostic(&'a ProviderDiagnostic),
    Value(ProviderValueDiagnostic<'a>),
}
impl<'a> ProviderEvent<'a> {
    pub fn failure(
        severity: EventLevel,
        context: ProviderContext,
        error: &'a ProviderError,
    ) -> Self {
        Self {
            severity,
            context: Some(context),
            body: EventInput::Failure(error),
        }
    }
    pub fn diagnostic(
        severity: EventLevel,
        context: ProviderContext,
        diagnostic: &'a ProviderDiagnostic,
    ) -> Self {
        Self {
            severity,
            context: Some(context),
            body: EventInput::Diagnostic(diagnostic),
        }
    }
    pub fn value(severity: EventLevel, value: ProviderValueDiagnostic<'a>) -> Self {
        Self {
            severity,
            context: None,
            body: EventInput::Value(value),
        }
    }
    #[cfg(target_arch = "wasm32")]
    pub fn into_wire(self) -> wire::Event {
        let Self {
            severity,
            context,
            body,
        } = self;
        wire::Event {
            severity,
            context: context.map(|context| context.0.wire_text()),
            body: match body {
                EventInput::Failure(error) => wire::Payload::Failure(error.to_wire()),
                EventInput::Diagnostic(diagnostic) => {
                    wire::Payload::Diagnostic(diagnostic.opaque().wire_text())
                }
                EventInput::Value(value) => wire::Payload::Value(value.to_wire()),
            },
        }
    }
}

/// An untrusted candidate, including its advisory origin and claimed address.
pub struct ProviderValueDiagnostic<'a> {
    id: Option<&'a protocol::ResourceId>,
    path: &'a [String],
    origin: ValueOrigin,
    input: ValueInput<'a>,
}
enum ValueInput<'a> {
    Typed(&'a protocol::Value),
    Json(&'a serde_json::Value),
}
impl<'a> ProviderValueDiagnostic<'a> {
    pub fn typed(
        id: Option<&'a protocol::ResourceId>,
        path: &'a [String],
        origin: ValueOrigin,
        input: &'a protocol::Value,
    ) -> Self {
        Self {
            id,
            path,
            origin,
            input: ValueInput::Typed(input),
        }
    }
    pub fn json(
        id: Option<&'a protocol::ResourceId>,
        path: &'a [String],
        origin: ValueOrigin,
        input: &'a serde_json::Value,
    ) -> Self {
        Self {
            id,
            path,
            origin,
            input: ValueInput::Json(input),
        }
    }
    fn to_wire(&self) -> wire::ValueDiagnostic {
        wire::ValueDiagnostic {
            id: self.id.cloned(),
            path: self.path.to_vec(),
            origin: self.origin,
            input: match self.input {
                ValueInput::Typed(value) => wire::DiagnosticValue::Typed(value.clone()),
                ValueInput::Json(value) => wire::DiagnosticValue::Json(value.to_string()),
            },
        }
    }
}

type EventCallback = Arc<dyn Fn(BoundProviderEvent) + Send + Sync>;
#[derive(Clone)]
pub(super) enum EventSink {
    Default,
    Custom(EventCallback),
}

pub(super) type ProtectedAddresses = HashMap<ResourceId, AddressProtection>;
#[derive(Clone, Debug)]
pub(super) enum AddressProtection {
    Checked(Vec<Vec<String>>),
    Unclassified,
}
impl AddressProtection {
    pub(super) fn include_checked(&mut self, mut additional: Vec<Vec<String>>) {
        match self {
            Self::Checked(paths) => {
                paths.append(&mut additional);
                paths.sort();
                paths.dedup();
            }
            Self::Unclassified => *self = Self::Checked(additional),
        }
    }
    fn paths(&self) -> &[Vec<String>] {
        match self {
            Self::Checked(paths) => paths,
            Self::Unclassified => &[],
        }
    }
}

struct ProviderTextScope {
    secrets: RedactionSet,
    addresses: RwLock<ProtectedAddresses>,
}
#[derive(Clone)]
struct Presentation {
    provider: String,
    schemas: Arc<RwLock<Option<SchemaRegistry>>>,
    sink: EventSink,
    host: Arc<()>,
}
#[derive(Default)]
struct RegistrationBudget {
    bytes: usize,
    values: usize,
}
struct EventQuota {
    count: usize,
    window: Instant,
    window_count: usize,
}
impl Default for EventQuota {
    fn default() -> Self {
        Self {
            count: 0,
            window: Instant::now(),
            window_count: 0,
        }
    }
}
impl EventQuota {
    fn admit(&mut self) -> bool {
        if self.window.elapsed() >= Duration::from_secs(1) {
            self.window = Instant::now();
            self.window_count = 0;
        }
        if self.count >= MAX_EVENTS || self.window_count >= MAX_EVENTS_PER_SECOND {
            return false;
        }
        self.count += 1;
        self.window_count += 1;
        true
    }
}
#[derive(Default)]
enum PanicExemption {
    #[default]
    Available,
    Used,
}
#[derive(Default)]
struct EventBudget {
    ordinary: EventQuota,
    verbose: EventQuota,
    panic_exemption: PanicExemption,
    suppressed: Option<SuppressionNotice>,
}
// A pending count retains its host delivery context, including for a channel
// that inherited presentation after its owner was created. No provider event
// text is retained. Shared Channel has only a weak owner and no budget pointer,
// so retaining it here cannot keep the diagnostic owner or budget alive.
struct SuppressionNotice {
    count: usize,
    shared: Arc<Channel>,
    presentation: Presentation,
}
impl EventBudget {
    fn admit(&mut self, level: EventLevel, panic: bool) -> bool {
        if panic && matches!(self.panic_exemption, PanicExemption::Available) {
            self.panic_exemption = PanicExemption::Used;
            return true;
        }
        let quota = match level {
            EventLevel::Error | EventLevel::Warn | EventLevel::Info => &mut self.ordinary,
            EventLevel::Debug | EventLevel::Trace => &mut self.verbose,
        };
        quota.admit()
    }
    fn suppress(&mut self, channel: &ProviderDiagnosticChannel) {
        let count = self
            .suppressed
            .as_ref()
            .map_or(1, |notice| notice.count.saturating_add(1));
        self.suppressed = Some(SuppressionNotice {
            count,
            shared: channel.shared.clone(),
            presentation: channel.presentation.clone(),
        });
    }
}
fn flush_suppressed(events: &Mutex<EventBudget>) {
    let notice = events
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .suppressed
        .take();
    if let Some(SuppressionNotice {
        count,
        shared,
        presentation,
    }) = notice
    {
        deliver_event(
            UnboundProviderEvent {
                severity: EventLevel::Warn,
                context: None,
                body: UnboundBody::Suppressed(count),
            },
            &shared.scope,
            &presentation,
        );
    }
}
struct DiagnosticLifetime {
    events: Arc<Mutex<EventBudget>>,
}
impl Drop for DiagnosticLifetime {
    fn drop(&mut self) {
        // In-flight emissions hold this same lifetime through Weak::upgrade,
        // so the final owner cannot drain the count before they finish.
        flush_suppressed(&self.events);
    }
}
impl fmt::Debug for DiagnosticOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("DiagnosticOwner").field(&()).finish()
    }
}
struct Channel {
    owner: std::sync::Weak<DiagnosticLifetime>,
    scope: ProviderTextScope,
}

/// The host owns liveness; emission handles retain only a weak reference.
#[derive(Clone)]
pub(crate) struct DiagnosticOwner(Arc<DiagnosticLifetime>);

/// A host-minted emission capability, without a writer or empty constructor.
#[derive(Clone)]
pub struct ProviderDiagnosticChannel {
    shared: Arc<Channel>,
    presentation: Presentation,
    registration: Arc<Mutex<RegistrationBudget>>,
    events: EventBinding,
}

#[derive(Clone)]
enum EventBinding {
    Session(Arc<Mutex<EventBudget>>),
    Call(Arc<Mutex<EventBudget>>),
}
struct ActiveCall {
    host: Arc<()>,
    events: Arc<Mutex<EventBudget>>,
}
tokio::task_local! { static ACTIVE_PROVIDER_CALL: ActiveCall; }

/// Ends one provider call's event budget, including trap and cancellation paths.
pub struct ProviderDiagnosticCall(Arc<Mutex<EventBudget>>);
impl Drop for ProviderDiagnosticCall {
    fn drop(&mut self) {
        // Drain the retained call budget, never an ambient task-local budget.
        flush_suppressed(&self.0);
    }
}
host_api! {
fn provider_http_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CARINA_WASI_HTTP_TRACE").is_ok_and(|value| value == "1"))
}
}
fn default_level_enabled(level: EventLevel) -> bool {
    match level {
        EventLevel::Error | EventLevel::Warn | EventLevel::Info => true,
        EventLevel::Debug | EventLevel::Trace => provider_http_trace_enabled(),
    }
}
impl fmt::Debug for ProviderDiagnosticChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderDiagnosticChannel")
    }
}
impl PartialEq for ProviderDiagnosticChannel {
    fn eq(&self, other: &Self) -> bool {
        self.presentation.provider == other.presentation.provider
            && self.shared.scope.secrets == other.shared.scope.secrets
    }
}
impl ProviderDiagnosticChannel {
    pub(super) fn new(
        secrets: RedactionSet,
        provider: String,
        addresses: ProtectedAddresses,
    ) -> (Self, DiagnosticOwner) {
        Self::with_sink(secrets, provider, addresses, EventSink::Default)
    }
    pub(super) fn with_sink(
        secrets: RedactionSet,
        provider: String,
        addresses: ProtectedAddresses,
        sink: EventSink,
    ) -> (Self, DiagnosticOwner) {
        let events = Arc::new(Mutex::new(EventBudget::default()));
        let owner = DiagnosticOwner(Arc::new(DiagnosticLifetime {
            events: events.clone(),
        }));
        let channel = Self {
            shared: Arc::new(Channel {
                owner: Arc::downgrade(&owner.0),
                scope: ProviderTextScope {
                    secrets,
                    addresses: RwLock::new(addresses),
                },
            }),
            presentation: Presentation {
                provider,
                schemas: Arc::new(RwLock::new(None)),
                sink,
                host: Arc::new(()),
            },
            registration: Arc::new(Mutex::new(RegistrationBudget::default())),
            events: EventBinding::Session(events),
        };
        (channel, owner)
    }
    pub(crate) fn set_schemas(&self, schemas: SchemaRegistry) {
        *self
            .presentation
            .schemas
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(schemas);
    }
    pub(super) fn set_schema_witness(
        &self,
        witness: &crate::schema::ValidatedProviderSchemaWitness,
    ) -> Result<(), crate::schema::SchemaRegistryError> {
        let mut schemas = SchemaRegistry::new();
        schemas.insert_provider_witness(&self.presentation.provider, witness)?;
        self.set_schemas(schemas);
        Ok(())
    }
    pub(super) fn inherit_host(&self, owner: &Self) -> Self {
        let presentation = owner.presentation.clone();
        let owner_has_schemas = owner
            .presentation
            .schemas
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if !owner_has_schemas {
            let schemas = self
                .presentation
                .schemas
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            *presentation
                .schemas
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = schemas;
        }
        let presentation = Presentation {
            sink: match &self.presentation.sink {
                EventSink::Custom(sink) => EventSink::Custom(sink.clone()),
                EventSink::Default => presentation.sink,
            },
            ..presentation
        };
        Self {
            shared: self.shared.clone(),
            presentation,
            registration: owner.registration.clone(),
            events: self.events.clone(),
        }
    }
    #[cfg_attr(
        not(feature = "guest-runtime"),
        allow(dead_code, reason = "Only the guest runtime replaces instances")
    )]
    pub(super) fn for_new_guest(&self, secrets: RedactionSet) -> (Self, DiagnosticOwner) {
        let (mut channel, owner) = Self::new(
            secrets,
            self.presentation.provider.clone(),
            ProtectedAddresses::default(),
        );
        channel.presentation = self.presentation.clone();
        (channel, owner)
    }
    pub(super) fn for_call(&self) -> (Self, ProviderDiagnosticCall) {
        let events = Arc::new(Mutex::new(EventBudget::default()));
        let call = Self {
            events: EventBinding::Call(events.clone()),
            ..self.clone()
        };
        (call, ProviderDiagnosticCall(events))
    }
    fn event_budget(&self) -> Arc<Mutex<EventBudget>> {
        match &self.events {
            // Guest exports carry their explicit call budget. A surrounding native
            // factory/dispatch scope must not combine separate guest exports.
            EventBinding::Call(events) => events.clone(),
            EventBinding::Session(fallback) => ACTIVE_PROVIDER_CALL
                .try_with(|call| {
                    Arc::ptr_eq(&call.host, &self.presentation.host).then(|| call.events.clone())
                })
                .ok()
                .flatten()
                .unwrap_or_else(|| fallback.clone()),
        }
    }
    fn call_context(&self) -> (ActiveCall, ProviderDiagnosticCall) {
        let (channel, guard) = self.for_call();
        (
            ActiveCall {
                host: self.presentation.host.clone(),
                events: channel.event_budget(),
            },
            guard,
        )
    }
    /// One native dispatch seam, including synchronous work before a future is
    /// returned. Task-local scopes isolate concurrent calls without mutating the
    /// session handles retained by a provider at construction.
    pub(crate) fn dispatch<'a, F: std::future::Future + Send + 'a>(
        &self,
        invoke: impl FnOnce() -> F,
    ) -> futures::future::BoxFuture<'a, F::Output> {
        let (context, guard) = self.call_context();
        let future = ACTIVE_PROVIDER_CALL.sync_scope(
            ActiveCall {
                host: context.host.clone(),
                events: context.events.clone(),
            },
            invoke,
        );
        Box::pin(ACTIVE_PROVIDER_CALL.scope(context, async move {
            let _guard = guard;
            future.await
        }))
    }
    pub(crate) fn dispatch_sync<T>(&self, invoke: impl FnOnce() -> T) -> T {
        let (context, guard) = self.call_context();
        ACTIVE_PROVIDER_CALL.sync_scope(context, || {
            let _guard = guard;
            invoke()
        })
    }
    pub(crate) fn include_address(&self, id: &ResourceId) {
        self.shared
            .scope
            .addresses
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(id.clone())
            .or_insert(AddressProtection::Unclassified);
    }
    pub(crate) fn include_ready_resource(&self, resource: &crate::provider::ProviderReadyResource) {
        self.include_checked_values(&resource.id, resource.as_resource().attributes.iter());
    }
    pub(crate) fn include_ready_data_source(
        &self,
        resource: &crate::provider::ProviderReadyDataSource,
    ) {
        self.include_checked_values(&resource.id, resource.attributes.iter());
    }
    fn include_checked_values<'a>(
        &self,
        id: &ResourceId,
        values: impl Iterator<Item = (&'a String, &'a Value)>,
    ) {
        include_protected_values(
            &mut self
                .shared
                .scope
                .addresses
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            id,
            values,
        );
    }
    pub fn emit(&self, event: ProviderEvent<'_>) {
        let ProviderEvent {
            severity,
            context,
            body,
        } = event;
        let body = match body {
            EventInput::Failure(error) => UnboundBody::Failure(error.clone()),
            EventInput::Diagnostic(diagnostic) => UnboundBody::Diagnostic(diagnostic.clone()),
            EventInput::Value(value) => UnboundBody::Value(value.to_wire()),
        };
        self.accept(UnboundProviderEvent {
            severity,
            context: context.map(|context| context.0),
            body,
        });
    }
    /// Host transport ingestion; the decoder remains in host confinement.
    #[cfg(feature = "guest-runtime")]
    pub fn receive_wire(
        &self,
        event: wire::Event,
        decode_failure: fn(protocol::ProviderError) -> ProviderError,
    ) {
        let panic = matches!(&event.body, wire::Payload::Panic(_));
        let wire::Event {
            severity,
            context,
            body,
        } = event;
        let severity = if panic { EventLevel::Error } else { severity };
        self.accept_decoded(severity, panic, || {
            let body = match body {
                wire::Payload::Failure(error) => {
                    if wire_error_bytes(&error)? > MAX_EVENT_INPUT_BYTES {
                        return None;
                    }
                    UnboundBody::Failure(decode_failure(error))
                }
                wire::Payload::Diagnostic(text) => {
                    UnboundBody::Diagnostic(ProviderDiagnostic::message(ProviderText::new(text)))
                }
                wire::Payload::Panic(panic) => UnboundBody::Panic(panic),
                wire::Payload::Value(value) => UnboundBody::Value(value),
                wire::Payload::Http(http) => UnboundBody::Http(http),
            };
            Some(UnboundProviderEvent {
                severity,
                context: context.map(ProviderText::new),
                body,
            })
        });
    }
    fn accept(&self, event: UnboundProviderEvent) {
        #[cfg(feature = "guest-runtime")]
        let panic = matches!(&event.body, UnboundBody::Panic(_));
        #[cfg(not(feature = "guest-runtime"))]
        let panic = false;
        self.accept_decoded(event.severity, panic, || Some(event));
    }
    fn accept_decoded(
        &self,
        level: EventLevel,
        panic: bool,
        decode: impl FnOnce() -> Option<UnboundProviderEvent>,
    ) {
        let Some(_live_context) = self.shared.owner.upgrade() else {
            return;
        };
        // Custom sinks are explicit observers of every level, independent of
        // stderr's trace opt-in. Filter default output before charging either
        // quota; hidden events also contribute nothing to the suppression notice.
        let enabled = match &self.presentation.sink {
            EventSink::Default => default_level_enabled(level),
            EventSink::Custom(_) => true,
        };
        if !enabled {
            return;
        }
        let events = self.event_budget();
        {
            let mut budget = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !budget.admit(level, panic) {
                budget.suppress(self);
                return;
            }
        }
        if !decode().is_some_and(|event| self.deliver(event)) {
            let mut budget = events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            budget.suppress(self);
        }
    }
    fn deliver(&self, event: UnboundProviderEvent) -> bool {
        deliver_event(event, &self.shared.scope, &self.presentation)
    }
    pub fn register_protected(
        &self,
        material: wire::ProtectedMaterial,
    ) -> Result<(), wire::RegistrationError> {
        let _live_context = self
            .shared
            .owner
            .upgrade()
            .ok_or(wire::RegistrationError::ClosedContext)?;
        let mut budget = self
            .registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let values = self
            .shared
            .scope
            .secrets
            .unregistered_values(material.values());
        let bytes = values
            .iter()
            .try_fold(0usize, |sum, value| sum.checked_add(value.len()))
            .ok_or(wire::RegistrationError::LimitExceeded)?;
        if bytes > MAX_REGISTERED_BYTES.saturating_sub(budget.bytes)
            || values.len() > MAX_REGISTERED_VALUES.saturating_sub(budget.values)
        {
            return Err(wire::RegistrationError::LimitExceeded);
        }
        let mut added = RedactionSet::before_request();
        for value in &values {
            added.insert(value);
        }
        added.sort();
        self.shared.scope.secrets.extend(&added);
        budget.bytes += bytes;
        budget.values += values.len();
        Ok(())
    }
}

fn deliver_event(
    event: UnboundProviderEvent,
    scope: &ProviderTextScope,
    presentation: &Presentation,
) -> bool {
    if let Some(event) = bind_event(event, scope, presentation) {
        match &presentation.sink {
            EventSink::Default => write_default_event(event),
            EventSink::Custom(sink) => sink(event),
        }
        true
    } else {
        false
    }
}

struct UnboundProviderEvent {
    severity: EventLevel,
    context: Option<ProviderText>,
    body: UnboundBody,
}
enum UnboundBody {
    Failure(ProviderError),
    Suppressed(usize),
    Diagnostic(ProviderDiagnostic),
    Value(wire::ValueDiagnostic),
    #[cfg(feature = "guest-runtime")]
    Panic(wire::PanicDiagnostic),
    #[cfg(feature = "guest-runtime")]
    Http(wire::HttpDiagnostic),
}

/// Only the private binding kernel can mint this type.
///
/// ```compile_fail,E0451
/// use carina_core::provider_text::BoundProviderEvent;
/// let _ = BoundProviderEvent { severity: todo!(), segments: vec![], failure: None, diagnostic: None, secrets: todo!(), notice: None };
/// ```
/// ```compile_fail,E0603
/// use carina_core::provider_text::events::bind_event;
/// ```
pub struct BoundProviderEvent {
    severity: EventLevel,
    segments: Vec<ErrorText<Bound>>,
    failure: Option<HostError>,
    diagnostic: Option<HostDiagnostic>,
    secrets: RedactionSet,
    notice: Option<usize>,
}
impl BoundProviderEvent {
    pub fn severity(&self) -> EventLevel {
        self.severity
    }
    pub fn failure(&self) -> Option<&HostError> {
        self.failure.as_ref()
    }
    pub fn diagnostic(&self) -> Option<&HostDiagnostic> {
        self.diagnostic.as_ref()
    }
}

/// Presentation accepts exactly a kernel-minted event, including for value diagnostics.
///
/// ```compile_fail,E0308
/// use carina_core::provider_text::{present_event, ErrorText, Bound};
/// fn reject(error: &ErrorText<Bound>) { let _ = present_event(error); }
/// ```
/// ```compile_fail,E0308
/// use carina_core::provider_text::{present_event, ProviderText, ProviderDiagnostic};
/// let _ = present_event(&ProviderDiagnostic::message(ProviderText::new("unbound")));
/// ```
/// ```
/// use carina_core::provider_text::{present_event, BoundProviderEvent};
/// fn display(event: &BoundProviderEvent) -> String { present_event(event).to_string() }
/// ```
pub fn present_event(event: &BoundProviderEvent) -> ProviderEventPresentation<'_> {
    ProviderEventPresentation(event)
}
pub struct ProviderEventPresentation<'a>(&'a BoundProviderEvent);
impl fmt::Display for ProviderEventPresentation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(count) = self.0.notice {
            return write!(f, "{count} provider events suppressed");
        }
        let mut inventories = RedactionContext::default();
        inventories.include(&self.0.secrets);
        for segment in &self.0.segments {
            inventories.extend(&segment.redaction_context());
        }
        let secrets = inventories
            .snapshot()
            .unwrap_or_else(|| self.0.secrets.clone());
        let mut stream = RedactionStream::new(secrets);
        let mut text = DiagnosticTextBuilder::default();
        for segment in &self.0.segments {
            stream.push_str(&segment.redacted_str(), &mut |chunk| text.push_str(chunk));
        }
        stream.flush(&mut |chunk| text.push_str(chunk));
        text.finish().fmt(f)
    }
}

// The shared kernel creates only Provider segments. Returned failures and events
// both enter here; generic Bound constructors and HostText have no role here.
pub(super) fn bind_text(text: ProviderText, secrets: &RedactionSet) -> ErrorText<Bound> {
    ErrorText {
        content: TextContent::Bound(vec![Segment::Provider {
            text,
            secrets: secrets.clone(),
        }]),
        projection: Default::default(),
        state: std::marker::PhantomData,
    }
}
fn text_bytes(text: &ProviderText) -> usize {
    text.0.iter().fold(0usize, |total, part| {
        total.saturating_add(match part {
            OpaquePart::Text(text) => text.len(),
            OpaquePart::Status(_) => 5,
            OpaquePart::Protected { text, secrets: _ } => text_bytes(text),
        })
    })
}
fn bind_event(
    mut event: UnboundProviderEvent,
    scope: &ProviderTextScope,
    presentation: &Presentation,
) -> Option<BoundProviderEvent> {
    let mut remaining = match &mut event.body {
        #[cfg(feature = "guest-runtime")]
        UnboundBody::Panic(wire::PanicDiagnostic {
            message,
            file,
            line,
            column,
        }) => {
            let input_bytes = event
                .context
                .as_ref()
                .map_or(0, text_bytes)
                .saturating_add(message.as_ref().map_or(0, String::len))
                .saturating_add(file.as_ref().map_or(0, String::len))
                .saturating_add(usize::from(line.is_some()) * 11)
                .saturating_add(usize::from(column.is_some()) * 11);
            if input_bytes > MAX_PANIC_INPUT_BYTES {
                event.context = event
                    .context
                    .take()
                    .map(|_context| ProviderText::new(crate::schema::SECRET_VALUE_MASK));
                for text in [message, file].into_iter().flatten() {
                    *text = crate::schema::SECRET_VALUE_MASK.to_owned();
                }
            }
            // Host scope and fixed framing do not consume the guest allowance.
            MAX_PANIC_INPUT_BYTES
                .checked_add(presentation.provider.len())?
                .checked_add(64)?
        }
        UnboundBody::Suppressed(_) => usize::MAX,
        UnboundBody::Failure(_) | UnboundBody::Diagnostic(_) | UnboundBody::Value(_) => {
            MAX_EVENT_INPUT_BYTES
        }
        #[cfg(feature = "guest-runtime")]
        UnboundBody::Http(_) => MAX_EVENT_INPUT_BYTES,
    };
    let UnboundProviderEvent {
        severity,
        context,
        body,
    } = event;
    let mut segments = Vec::new();
    let mut push = |text: ProviderText| -> Option<()> {
        remaining = remaining.checked_sub(text_bytes(&text))?;
        segments.push(bind_text(text, &scope.secrets));
        Some(())
    };
    push(ProviderText::new(match severity {
        EventLevel::Error => "error",
        EventLevel::Warn => "warn",
        EventLevel::Info => "info",
        EventLevel::Debug => "debug",
        EventLevel::Trace => "trace",
    }))?;
    push(ProviderText::new(format!(" [{}] ", presentation.provider)))?;
    if let Some(context) = context {
        push(context)?;
        push(ProviderText::new(": "))?;
    }
    let mut failure = None;
    let mut captured_diagnostic = None;
    match body {
        UnboundBody::Suppressed(count) => {
            return Some(BoundProviderEvent {
                severity,
                segments: Vec::new(),
                failure: None,
                diagnostic: None,
                secrets: scope.secrets.clone(),
                notice: Some(count),
            });
        }
        UnboundBody::Failure(error) => {
            push(error.diagnostic_text())?;
            failure = Some(error.with_secrets(&scope.secrets));
        }
        UnboundBody::Diagnostic(diagnostic) => {
            push(diagnostic.opaque())?;
            captured_diagnostic = Some(diagnostic.with_secrets(&scope.secrets));
        }
        UnboundBody::Value(wire::ValueDiagnostic {
            id,
            path,
            origin,
            input,
        }) => {
            if path.len() > MAX_EVENT_DEPTH {
                return None;
            }
            let address_bytes = path
                .iter()
                .try_fold(0usize, |total, part| total.checked_add(part.len()))?
                .checked_add(id.as_ref().map_or(0, |id| {
                    id.provider
                        .len()
                        .saturating_add(id.resource_type.len())
                        .saturating_add(id.identity.len())
                }))?;
            if address_bytes > MAX_EVENT_INPUT_BYTES {
                return None;
            }
            let value = match input {
                wire::DiagnosticValue::Typed(value) => Some(value),
                wire::DiagnosticValue::Masked => None,
                wire::DiagnosticValue::Json(json) => {
                    if json.len() > MAX_EVENT_INPUT_BYTES {
                        return None;
                    }
                    json_decode_budget(&json)?;
                    Some(protocol::Value::from_json(serde_json::from_str(&json).ok()?).ok()?)
                }
            };
            if let Some(value) = &value {
                candidate_budget(value)?;
            }
            let addresses = scope
                .addresses
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let resolved = id.as_ref().and_then(|guest| {
                addresses.iter().find(|(host, _paths)| {
                    host.provider == guest.provider
                        && host.resource_type == guest.resource_type
                        && host.identity_str() == Some(guest.identity.as_str())
                })
            });
            let schemas = presentation
                .schemas
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let schema = resolved.and_then(|(host, _paths)| {
                schemas.as_ref().and_then(|schemas| {
                    schemas
                        .get(&host.provider, &host.resource_type, SchemaKind::Resource)
                        .or_else(|| {
                            schemas.get(&host.provider, &host.resource_type, SchemaKind::DataSource)
                        })
                })
            });
            if let Some((host, _paths)) = resolved {
                push(ProviderText::new(host.to_string()))?;
                push(ProviderText::new(" "))?;
            }
            push(ProviderText::new(path.join(".")))?;
            push(ProviderText::new(": "))?;
            let mut protection = origin == ValueOrigin::Unclassified;
            if let Some((_host, coverage)) = resolved {
                protection |= match coverage {
                    AddressProtection::Checked(paths) => {
                        paths.iter().any(|parent| path.starts_with(parent))
                    }
                    AddressProtection::Unclassified => true,
                };
            }
            let mut effective = None;
            if let Some(schema) = schema {
                let schema = schema.schema();
                for end in 1..=path.len() {
                    if let Some(node) = schema.effective_node_at_path(&path[..end]) {
                        protection |= node.is_write_only();
                    }
                }
                effective = schema.effective_type_at_attribute_path(&path).ok();
            }
            protection |= resolved.is_none() || effective.is_none();
            if let Some(value) = value {
                ValueBinding {
                    protected: resolved.map_or(&[], |(_host, coverage)| coverage.paths()),
                    path,
                    push: &mut push,
                }
                .bind(&value, effective, protection)?;
            } else {
                push(ProviderText::new(crate::schema::SECRET_VALUE_MASK))?;
            }
        }
        #[cfg(feature = "guest-runtime")]
        UnboundBody::Panic(wire::PanicDiagnostic {
            message,
            file,
            line,
            column,
        }) => {
            if let Some(message) = message {
                push(ProviderText::new(message))?;
            }
            if let Some(file) = file {
                push(ProviderText::new(" at "))?;
                push(ProviderText::new(file))?;
            }
            if let Some(line) = line {
                push(ProviderText::new(format!(":{line}")))?;
            }
            if let Some(column) = column {
                push(ProviderText::new(format!(":{column}")))?;
            }
        }
        #[cfg(feature = "guest-runtime")]
        UnboundBody::Http(wire::HttpDiagnostic {
            method,
            uri,
            headers,
            request_body_bytes,
            response_body_bytes,
            status,
            transport_code,
            phases_ms,
        }) => {
            use carina_provider_protocol::http_trace::{TraceHeaders, TraceUri};
            if headers.len() > MAX_EVENT_NODES {
                return None;
            }
            let input_bytes = headers.iter().try_fold(
                method.len().checked_add(uri.len())?,
                |size, (name, value)| size.checked_add(name.len())?.checked_add(value.len()),
            )?;
            if input_bytes > MAX_EVENT_INPUT_BYTES {
                return None;
            }
            push(ProviderText::new(format!(
                "carina-wasi-http-trace method={method} uri={} body_in={request_body_bytes} headers=[{}]",
                TraceUri::new(&uri),
                TraceHeaders::new(&headers)
            )))?;
            if let Some(status) = status {
                push(ProviderText::new(format!(" status={status}")))?;
            }
            if let Some(bytes) = response_body_bytes {
                push(ProviderText::new(format!(" body_out={bytes}")))?;
            }
            if let Some(code) = transport_code {
                push(ProviderText::new(match code {
                    wire::HttpTransportCode::Dns => " transport=dns",
                    wire::HttpTransportCode::Connect => " transport=connect",
                    wire::HttpTransportCode::Tls => " transport=tls",
                    wire::HttpTransportCode::Timeout => " transport=timeout",
                    wire::HttpTransportCode::Protocol => " transport=protocol",
                    wire::HttpTransportCode::Body => " transport=body",
                }))?;
            }
            for (name, ms) in [
                "setup",
                "handle",
                "write_body",
                "body_finish",
                "pollable_block",
                "get_response",
                "read_body",
            ]
            .into_iter()
            .zip(phases_ms)
            {
                push(ProviderText::new(format!(" {name}_ms={ms}")))?;
            }
        }
    }
    Some(BoundProviderEvent {
        severity,
        segments,
        failure,
        diagnostic: captured_diagnostic,
        secrets: scope.secrets.clone(),
        notice: None,
    })
}

fn candidate_budget(value: &protocol::Value) -> Option<()> {
    let mut pending = vec![(value, 0)];
    let mut nodes = MAX_EVENT_NODES;
    let mut bytes = MAX_EVENT_INPUT_BYTES;
    while let Some((value, depth)) = pending.pop() {
        if depth > MAX_EVENT_DEPTH {
            return None;
        }
        nodes = nodes.checked_sub(1)?;
        let length = match value {
            protocol::Value::Bool(_) | protocol::Value::Int(_) | protocol::Value::Float(_) => 32,
            protocol::Value::String(text) => text.len(),
            protocol::Value::StringList(items) => {
                nodes = nodes.checked_sub(items.len())?;
                items
                    .iter()
                    .try_fold(0usize, |n, item| n.checked_add(item.len()))?
            }
            protocol::Value::List(items) => {
                if items.len() > nodes {
                    return None;
                }
                pending.extend(items.iter().map(|item| (item, depth + 1)));
                0
            }
            protocol::Value::Map(items) => {
                if items.len() > nodes {
                    return None;
                }
                pending.extend(items.values().map(|item| (item, depth + 1)));
                items
                    .keys()
                    .try_fold(0usize, |n, key| n.checked_add(key.len()))?
            }
        };
        bytes = bytes.checked_sub(length)?;
    }
    Some(())
}

struct ValueBinding<'a, F> {
    protected: &'a [Vec<String>],
    path: Vec<String>,
    push: &'a mut F,
}
impl<F: FnMut(ProviderText) -> Option<()>> ValueBinding<'_, F> {
    fn bind(
        &mut self,
        value: &protocol::Value,
        ty: Option<EffectiveType<'_>>,
        masked: bool,
    ) -> Option<()> {
        if masked
            || self
                .protected
                .iter()
                .any(|parent| self.path.starts_with(parent))
        {
            return (self.push)(ProviderText::new(crate::schema::SECRET_VALUE_MASK));
        }
        match value {
            protocol::Value::Bool(value) => (self.push)(ProviderText::new(value.to_string())),
            protocol::Value::Int(value) => (self.push)(ProviderText::new(value.to_string())),
            protocol::Value::Float(value) => (self.push)(ProviderText::new(value.to_string())),
            protocol::Value::String(value) => {
                (self.push)(ProviderText::new("\""))?;
                (self.push)(ProviderText::new(value))?;
                (self.push)(ProviderText::new("\""))
            }
            protocol::Value::StringList(values) => {
                (self.push)(ProviderText::new("["))?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        (self.push)(ProviderText::new(", "))?;
                    }
                    self.path.push(index.to_string());
                    let hidden = self
                        .protected
                        .iter()
                        .any(|parent| self.path.starts_with(parent));
                    (self.push)(ProviderText::new(if hidden {
                        crate::schema::SECRET_VALUE_MASK
                    } else {
                        value
                    }))?;
                    self.path.pop();
                }
                (self.push)(ProviderText::new("]"))
            }
            protocol::Value::List(values) => {
                let element = ty.as_ref().and_then(EffectiveType::list_element);
                (self.push)(ProviderText::new("["))?;
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        (self.push)(ProviderText::new(", "))?;
                    }
                    self.path.push(index.to_string());
                    self.bind(value, element.clone(), element.is_none())?;
                    self.path.pop();
                }
                (self.push)(ProviderText::new("]"))
            }
            protocol::Value::Map(values) => {
                (self.push)(ProviderText::new("{"))?;
                for (index, (key, value)) in values.iter().enumerate() {
                    if index > 0 {
                        (self.push)(ProviderText::new(", "))?;
                    }
                    let field = ty.as_ref().and_then(|ty| {
                        ty.field_candidates()
                            .into_iter()
                            .find(|field| field.name() == key)
                    });
                    let child = field
                        .as_ref()
                        .map(|field| field.value_type())
                        .or_else(|| ty.as_ref().and_then(EffectiveType::map_value));
                    if child.is_none() {
                        (self.push)(ProviderText::new(crate::schema::SECRET_VALUE_MASK))?;
                        continue;
                    }
                    (self.push)(ProviderText::new(key))?;
                    (self.push)(ProviderText::new(": "))?;
                    let masked = field.as_ref().is_some_and(|field| field.is_write_only());
                    self.path.push(key.clone());
                    self.bind(value, child, masked)?;
                    self.path.pop();
                }
                (self.push)(ProviderText::new("}"))
            }
        }
    }
}

pub(super) fn protected_paths<P: crate::resource::ValueKind>(
    value: &Value<P>,
    path: &mut Vec<String>,
    out: &mut Vec<Vec<String>>,
) {
    match value {
        Value::Deferred(_opaque) => out.push(path.clone()),
        Value::Concrete(ConcreteValue::String(value)) => {
            if value.is_diagnostic_sensitive() {
                out.push(path.clone());
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for (key, value) in values {
                path.push(key.clone());
                protected_paths(value, path, out);
                path.pop();
            }
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            for (index, value) in values.iter().enumerate() {
                path.push(index.to_string());
                protected_paths(value, path, out);
                path.pop();
            }
        }
        Value::Concrete(ConcreteValue::StringList(values)) => {
            for (index, value) in values.iter().enumerate() {
                if value.is_diagnostic_sensitive() {
                    path.push(index.to_string());
                    out.push(path.clone());
                    path.pop();
                }
            }
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(_identifier)) => {}
        Value::Concrete(ConcreteValue::CanonicalEnum(_value)) => {}
        Value::Concrete(ConcreteValue::Int(_value)) => {}
        Value::Concrete(ConcreteValue::Float(_value)) => {}
        Value::Concrete(ConcreteValue::Bool(_value)) => {}
        Value::Concrete(ConcreteValue::Duration(_value)) => {}
    }
}

pub(super) fn include_protected_values<'a, P: crate::resource::ValueKind + 'a>(
    addresses: &mut ProtectedAddresses,
    id: &ResourceId,
    values: impl IntoIterator<Item = (&'a String, &'a Value<P>)>,
) {
    let mut paths = Vec::new();
    for (name, value) in values {
        protected_paths(value, &mut vec![name.clone()], &mut paths);
    }
    addresses
        .entry(id.clone())
        .or_insert(AddressProtection::Unclassified)
        .include_checked(paths);
}

#[cfg(feature = "guest-runtime")]
fn wire_error_bytes(error: &protocol::ProviderError) -> Option<usize> {
    let protocol::ProviderError {
        kind: _kind,
        retryability: _retryability,
        message,
        resource_id,
        cause,
        provider_name,
        operation,
        status: _status,
        code,
        request_id,
    } = error;
    let mut bytes = message.len();
    for field in [cause, provider_name, operation, code, request_id]
        .into_iter()
        .flatten()
    {
        bytes = bytes.checked_add(field.len())?;
    }
    if let Some(protocol::ResourceId {
        provider,
        resource_type,
        identity,
    }) = resource_id
    {
        for field in [provider, resource_type, identity] {
            bytes = bytes.checked_add(field.len())?;
        }
    }
    Some(bytes)
}
fn json_decode_budget(json: &str) -> Option<()> {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in json.bytes() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else {
            match byte {
                b'"' => quoted = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth > MAX_EVENT_DEPTH {
                        return None;
                    }
                }
                b']' | b'}' => depth = depth.checked_sub(1)?,
                _ => {}
            }
        }
    }
    Some(())
}

fn write_default_event(event: BoundProviderEvent) {
    let mut writer = if event.notice.is_some() {
        RedactingLineWriter::for_metadata(std::io::stderr())
    } else {
        RedactingLineWriter::new(&event.secrets, std::io::stderr())
    };
    if writer.push_str(&present_event(&event).to_string()).is_ok() {
        let _ = writer.push_str("\n");
        let _ = writer.finish();
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;

    #[test]
    fn writer_preserves_admission_and_warn_suppression_notice() {
        const CHILD: &str = "CARINA_4334_FIX4_WRITER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let session = crate::provider::ProviderPlan::state_only()
                .session_builder("writer-test", None, None)
                .with_diagnostic_sink(write_default_event)
                .build();
            let (inputs, call) = session.guest_inventory().for_diagnostic_call();
            let channel = inputs.diagnostics();
            let trace = ProviderDiagnostic::message(ProviderText::new("admitted-trace-canary"));
            channel.emit(ProviderEvent::diagnostic(
                EventLevel::Trace,
                "writer".into(),
                &trace,
            ));
            let warning = ProviderDiagnostic::message(ProviderText::new("admitted-warning-canary"));
            for _ in 0..400 {
                channel.emit(ProviderEvent::diagnostic(
                    EventLevel::Warn,
                    "writer".into(),
                    &warning,
                ));
            }
            drop(call);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "provider_text::events::writer_tests::writer_preserves_admission_and_warn_suppression_notice", "--nocapture"])
            .env(CHILD, "1")
            .env_remove("CARINA_WASI_HTTP_TRACE")
            .output().unwrap();
        let text = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(text.contains("admitted-trace-canary"), "{text}");
        assert!(text.contains("admitted-warning-canary"), "{text}");
        assert_eq!(
            text.matches("provider events suppressed").count(),
            1,
            "{text}"
        );
    }
}
