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
const MAX_EVENTS: usize = 256;
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

type EventSink = Arc<dyn Fn(BoundProviderEvent) + Send + Sync>;
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
    provider: String,
    addresses: RwLock<ProtectedAddresses>,
    schemas: RwLock<SchemaRegistry>,
}
struct Budget {
    count: usize,
    window: Instant,
    window_count: usize,
    registered_bytes: usize,
    registered_values: usize,
}
struct Channel {
    owner: std::sync::Weak<()>,
    scope: ProviderTextScope,
    sink: RwLock<EventSink>,
    budget: Mutex<Budget>,
}

/// The host owns liveness; provider emission handles retain only a weak reference.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DiagnosticOwner(Arc<()>);

/// A host-minted emission capability. There is no writer or empty constructor.
/// Neither emission nor registration returns displayable provider text.
#[derive(Clone)]
pub struct ProviderDiagnosticChannel(Arc<Channel>);
impl fmt::Debug for ProviderDiagnosticChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProviderDiagnosticChannel")
    }
}
impl PartialEq for ProviderDiagnosticChannel {
    fn eq(&self, other: &Self) -> bool {
        self.0.scope.provider == other.0.scope.provider
            && self.0.scope.secrets == other.0.scope.secrets
    }
}
impl ProviderDiagnosticChannel {
    pub(super) fn new(
        secrets: RedactionSet,
        provider: String,
        addresses: ProtectedAddresses,
    ) -> (Self, DiagnosticOwner) {
        let output_secrets = secrets.clone();
        let owner = DiagnosticOwner(Arc::new(()));
        let channel = Self(Arc::new(Channel {
            owner: Arc::downgrade(&owner.0),
            scope: ProviderTextScope {
                secrets,
                provider,
                addresses: RwLock::new(addresses),
                schemas: RwLock::new(SchemaRegistry::new()),
            },
            sink: RwLock::new(Arc::new(move |event| {
                let mut writer = RedactingLineWriter::new(&output_secrets, std::io::stderr());
                if writer.push_str(&present_event(&event).to_string()).is_ok() {
                    let _ = writer.push_str("\n");
                    let _ = writer.finish();
                }
            })),
            budget: Mutex::new(Budget {
                count: 0,
                window: Instant::now(),
                window_count: 0,
                registered_bytes: 0,
                registered_values: 0,
            }),
        }));
        (channel, owner)
    }
    pub(super) fn set_sink(&self, sink: impl Fn(BoundProviderEvent) + Send + Sync + 'static) {
        *self
            .0
            .sink
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(sink);
    }
    pub(crate) fn set_schemas(&self, schemas: SchemaRegistry) {
        *self
            .0
            .scope
            .schemas
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = schemas;
    }
    pub(crate) fn include_address(&self, id: &ResourceId) {
        self.0
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
        let mut additional = Vec::new();
        for (name, value) in values {
            protected_paths(value, &mut vec![name.clone()], &mut additional);
        }
        self.0
            .scope
            .addresses
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(id.clone())
            .or_insert(AddressProtection::Unclassified)
            .include_checked(additional);
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
    /// Transport ingestion exists only on the host; all decoded fields remain Unbound.
    #[cfg(feature = "guest-runtime")]
    pub fn receive_wire(
        &self,
        event: wire::Event,
        decode_failure: fn(protocol::ProviderError) -> ProviderError,
    ) {
        let wire::Event {
            severity,
            context,
            body,
        } = event;
        let body = match body {
            wire::Payload::Failure(error) => UnboundBody::Failure(decode_failure(error)),
            wire::Payload::Diagnostic(text) => {
                UnboundBody::Diagnostic(ProviderDiagnostic::message(ProviderText::new(text)))
            }
            wire::Payload::Panic(panic) => UnboundBody::Panic(panic),
            wire::Payload::Value(value) => UnboundBody::Value(value),
            wire::Payload::Http(http) => UnboundBody::Http(http),
        };
        self.accept(UnboundProviderEvent {
            severity,
            context: context.map(ProviderText::new),
            body,
        });
    }
    fn accept(&self, event: UnboundProviderEvent) {
        let Some(_live_context) = self.0.owner.upgrade() else {
            return;
        };
        {
            let mut budget = self
                .0
                .budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if budget.count >= MAX_EVENTS {
                return;
            }
            if budget.window.elapsed() >= Duration::from_secs(1) {
                budget.window = Instant::now();
                budget.window_count = 0;
            }
            if budget.window_count >= 64 {
                return;
            }
            budget.count += 1;
            budget.window_count += 1;
        }
        if let Some(event) = bind_event(event, &self.0.scope) {
            let sink = self
                .0
                .sink
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            sink(event);
        }
    }
    pub fn register_protected(
        &self,
        material: wire::ProtectedMaterial,
    ) -> Result<(), wire::RegistrationError> {
        let _live_context = self
            .0
            .owner
            .upgrade()
            .ok_or(wire::RegistrationError::ClosedContext)?;
        let values = match material {
            wire::ProtectedMaterial::AwsSession(wire::AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            }) => {
                let mut values = vec![access_key_id, secret_access_key];
                values.extend(session_token);
                values
            }
            wire::ProtectedMaterial::GithubToken(token) => vec![token],
            wire::ProtectedMaterial::SignedMaterial(wire::SignedMaterial {
                authorization,
                signature,
                query_secret_values,
            }) => {
                let mut values = vec![authorization];
                values.extend(signature);
                values.extend(query_secret_values);
                values
            }
        };
        let bytes = values
            .iter()
            .try_fold(0usize, |sum, value| sum.checked_add(value.len()))
            .ok_or(wire::RegistrationError::LimitExceeded)?;
        let mut budget = self
            .0
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bytes > MAX_REGISTERED_BYTES.saturating_sub(budget.registered_bytes)
            || values.len() > MAX_REGISTERED_VALUES.saturating_sub(budget.registered_values)
        {
            return Err(wire::RegistrationError::LimitExceeded);
        }
        let mut added = RedactionSet::before_request();
        for value in &values {
            added.insert(value);
        }
        added.sort();
        self.0.scope.secrets.extend(&added);
        budget.registered_bytes += bytes;
        budget.registered_values += values.len();
        Ok(())
    }
}

struct UnboundProviderEvent {
    severity: EventLevel,
    context: Option<ProviderText>,
    body: UnboundBody,
}
enum UnboundBody {
    Failure(ProviderError),
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
/// let _ = BoundProviderEvent { severity: todo!(), segments: vec![], failure: None, diagnostic: None, secrets: todo!() };
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
/// use carina_core::provider_text::present_event;
/// let _ = present_event(&String::from("unbound provider output"));
/// ```
/// ```compile_fail,E0308
/// use carina_core::provider_text::{present_event, ErrorText, Bound};
/// let _ = present_event(&ErrorText::<Bound>::new("arbitrary"));
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
    event: UnboundProviderEvent,
    scope: &ProviderTextScope,
) -> Option<BoundProviderEvent> {
    let UnboundProviderEvent {
        severity,
        context,
        body,
    } = event;
    let mut remaining = MAX_EVENT_INPUT_BYTES;
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
    push(ProviderText::new(format!(" [{}] ", scope.provider)))?;
    if let Some(context) = context {
        push(context)?;
        push(ProviderText::new(": "))?;
    }
    let mut failure = None;
    let mut captured_diagnostic = None;
    match body {
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
                wire::DiagnosticValue::Typed(value) => value,
                wire::DiagnosticValue::Json(json) => {
                    if json.len() > MAX_EVENT_INPUT_BYTES {
                        return None;
                    }
                    protocol::Value::from_json(serde_json::from_str(&json).ok()?).ok()?
                }
            };
            candidate_budget(&value)?;
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
            let schemas = scope
                .schemas
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let schema = resolved.and_then(|(host, _paths)| {
                schemas
                    .get(&host.provider, &host.resource_type, SchemaKind::Resource)
                    .or_else(|| {
                        schemas.get(&host.provider, &host.resource_type, SchemaKind::DataSource)
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
            ValueBinding {
                protected: resolved.map_or(&[], |(_host, coverage)| coverage.paths()),
                path,
                push: &mut push,
            }
            .bind(&value, effective, protection)?;
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
