//! Scoped, opt-in observations for the offline identity probe (`test-utils` only).
//! Raw observations can contain secrets; the facade controls their disclosure.

use std::cell::RefCell;
use std::hash::Hasher;
use std::rc::Rc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::parser::retained::SlotDeclarationId;
use crate::parser::{DeclarationLocation, ModuleCallId};
use crate::resource::{
    AnonymousIdentityModuleSource, BindingAddress, LoopOriginKind, Resource, ResourceIdentity,
    ValueKind,
};

/// Parser-minted declaration and expansion identities, independent of values,
/// generated addresses, user names, and the slice used by an identity pass.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ResourceKey {
    declaration: SlotDeclarationId,
    modules: Vec<ModuleCallId>,
    loops: Vec<(ResourceIdentity, LoopOriginKind)>,
}

impl ResourceKey {
    pub fn source(&self) -> (&std::path::Path, usize) {
        let (file, ordinal) = self.declaration.recording_source();
        (file.path(), ordinal)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleKey(Vec<ModuleCallId>);

impl ModuleKey {
    pub fn calls(&self) -> impl Iterator<Item = &ModuleCallId> {
        self.0.iter()
    }
    pub fn prefixes<'a>(calls: impl Iterator<Item = &'a ModuleCallId>) -> Vec<Self> {
        let mut path = Vec::new();
        calls
            .map(|call| {
                path.push(call.clone());
                Self(path.clone())
            })
            .collect()
    }
    pub fn owns(&self, resource: &ResourceKey) -> bool {
        resource.modules.starts_with(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InputOwner {
    Resource(ResourceKey),
    Module(ModuleKey),
    /// Both identities come directly from the producer, never a second
    /// implementation of its digest-to-address encoding.
    Loop {
        identity: ResourceIdentity,
        parent: Option<ResourceIdentity>,
    },
    SecretProducer(SecretProducer),
    Unattributed,
}

/// The production operation that owns secret material outside a resource pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretProducer {
    PayloadHash,
    PayloadVerification,
    SnapshotHash,
}
impl SecretProducer {
    pub fn name(self) -> &'static str {
        match self {
            Self::PayloadHash => "secret-payload-hash",
            Self::PayloadVerification => "secret-payload-verification",
            Self::SnapshotHash => "snapshot-secret-hash",
        }
    }
}

/// Structured labels retain source identity until the facade renders paths and
/// fingerprints expansion keys. They never require parsing a display string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputContext {
    Resource(Option<DeclarationLocation>),
    Module(ModuleCallId),
    SecretProducer(SecretProducer),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddressPart {
    Literal(String),
    MapKey(String),
}

struct AddressKey {
    address: String,
    parent: String,
    key: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashWrite {
    pub operation: &'static str,
    /// Typed integer calls use little endian; usize/isize use 64 bits. The
    /// original typed operation is forwarded unchanged to the real hasher.
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashFinish {
    pub operation: &'static str,
    pub after_writes: usize,
    pub output: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashInput {
    pub boundary: &'static str,
    pub owner: InputOwner,
    pub context: Vec<InputContext>,
    pub writes: Vec<HashWrite>,
    pub output: Option<Vec<u8>>,
    pub finishes: Vec<HashFinish>,
    pub feature_count: Option<usize>,
}

#[derive(Default)]
struct Frame {
    context: Vec<InputContext>,
    owner: Option<InputOwner>,
    modules: Vec<ModuleCallId>,
    inputs: Vec<HashInput>,
    address_keys: Vec<AddressKey>,
}

thread_local! {
    static ACTIVE: RefCell<Option<Rc<RefCell<Frame>>>> = const { RefCell::new(None) };
}

struct RecorderGuard {
    previous: Option<Rc<RefCell<Frame>>>,
}
impl Drop for RecorderGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = self.previous.take());
    }
}

/// Record synchronous work on this thread. Nested recorders are independent;
/// unwinding restores the preceding recorder. Inputs retain hasher call order.
pub fn record<R>(run: impl FnOnce() -> R) -> (R, Vec<HashInput>) {
    let frame = Rc::new(RefCell::new(Frame::default()));
    let previous = ACTIVE.with(|active| active.replace(Some(frame.clone())));
    let guard = RecorderGuard { previous };
    let result = run();
    drop(guard);
    let inputs = std::mem::take(&mut frame.borrow_mut().inputs);
    (result, inputs)
}

enum Restore {
    Owner(Option<InputOwner>),
    Module,
}

/// Guards own an Rc, so cannot be moved to another thread.
pub(crate) struct ContextGuard(Option<(Rc<RefCell<Frame>>, Restore)>);
impl Drop for ContextGuard {
    fn drop(&mut self) {
        if let Some((frame, restore)) = self.0.take() {
            let mut frame = frame.borrow_mut();
            match restore {
                Restore::Owner(owner) => {
                    frame.owner = owner;
                    frame.context.pop();
                }
                Restore::Module => {
                    frame.modules.pop();
                }
            }
        }
    }
}

fn owner_context(make: impl FnOnce(&Frame) -> (InputOwner, InputContext)) -> ContextGuard {
    let frame = ACTIVE.with(|active| active.borrow().clone());
    ContextGuard(frame.map(|frame| {
        let (owner, label) = make(&frame.borrow());
        let previous = {
            let mut active = frame.borrow_mut();
            active.context.push(label);
            active.owner.replace(owner)
        };
        (frame, Restore::Owner(previous))
    }))
}

pub(crate) fn resource_context<
    S: AnonymousIdentityModuleSource,
    P: ValueKind,
    B: BindingAddress,
    D,
>(
    resource: &Resource<S, P, B, D>,
) -> ContextGuard {
    owner_context(|_| {
        let owner = resource_key(resource)
            .map(InputOwner::Resource)
            .unwrap_or(InputOwner::Unattributed);
        let label = InputContext::Resource(resource_location(resource).cloned());
        (owner, label)
    })
}

pub(crate) fn module_context(id: &ModuleCallId) -> ContextGuard {
    owner_context(|frame| {
        let mut path = frame.modules.clone();
        path.push(id.clone());
        (
            InputOwner::Module(ModuleKey(path)),
            InputContext::Module(id.clone()),
        )
    })
}

pub(crate) fn enter_module(id: &ModuleCallId) -> ContextGuard {
    let frame = ACTIVE.with(|active| active.borrow().clone());
    ContextGuard(frame.map(|frame| {
        frame.borrow_mut().modules.push(id.clone());
        (frame, Restore::Module)
    }))
}

/// A lightweight handle into the recorder. Insert at construction, mutate in
/// place, and move the completed vector out once: no Drop clone or reordering.
#[derive(Clone)]
pub(crate) struct Capture {
    frame: Rc<RefCell<Frame>>,
    index: usize,
}
impl Capture {
    fn new(boundary: &'static str) -> Option<Self> {
        ACTIVE.with(|active| {
            let frame = active.borrow().clone()?;
            let index = {
                let mut active = frame.borrow_mut();
                let input = HashInput {
                    boundary,
                    owner: active.owner.clone().unwrap_or(InputOwner::Unattributed),
                    context: active.context.clone(),
                    writes: Vec::new(),
                    output: None,
                    finishes: Vec::new(),
                    feature_count: None,
                };
                let index = active.inputs.len();
                active.inputs.push(input);
                index
            };
            Some(Self { frame, index })
        })
    }
    fn write(&self, operation: &'static str, bytes: &[u8]) {
        self.frame.borrow_mut().inputs[self.index]
            .writes
            .push(HashWrite {
                operation,
                bytes: bytes.to_vec(),
            });
    }
    pub(crate) fn finish(&self, operation: &'static str, bytes: &[u8]) {
        let mut frame = self.frame.borrow_mut();
        let input = &mut frame.inputs[self.index];
        input.finishes.push(HashFinish {
            operation,
            after_writes: input.writes.len(),
            output: bytes.to_vec(),
        });
        input.output = Some(bytes.to_vec());
    }
    pub(crate) fn loop_result(&self, identity: &ResourceIdentity, parent: Option<&str>) {
        self.frame.borrow_mut().inputs[self.index].owner = InputOwner::Loop {
            identity: identity.clone(),
            parent: parent.map(|parent| {
                ResourceIdentity::try_from(parent.to_owned())
                    .expect("loop parent is a system identity")
            }),
        };
    }
}

pub(crate) struct RecordingHasher<H> {
    inner: H,
    capture: Option<Capture>,
}
impl<H> RecordingHasher<H> {
    pub(crate) fn new(inner: H, boundary: &'static str) -> Self {
        Self {
            inner,
            capture: Capture::new(boundary),
        }
    }
}
macro_rules! integer_writes {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method(&mut self, value: $ty) {
            self.inner.$method(value);
            if let Some(capture) = &self.capture { capture.write(stringify!($method), &value.to_le_bytes()); }
        }
    )*};
}
impl<H: Hasher> Hasher for RecordingHasher<H> {
    fn finish(&self) -> u64 {
        let output = self.inner.finish();
        if let Some(capture) = &self.capture {
            capture.finish("finish", &output.to_le_bytes());
        }
        output
    }
    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
        if let Some(capture) = &self.capture {
            capture.write("write", bytes);
        }
    }
    integer_writes! {
        write_u8(u8), write_u16(u16), write_u32(u32), write_u64(u64), write_u128(u128),
        write_i8(i8), write_i16(i16), write_i32(i32), write_i64(i64), write_i128(i128),
    }
    fn write_usize(&mut self, value: usize) {
        self.inner.write_usize(value);
        if let Some(capture) = &self.capture {
            capture.write("write_usize", &(value as u64).to_le_bytes());
        }
    }
    fn write_isize(&mut self, value: isize) {
        self.inner.write_isize(value);
        if let Some(capture) = &self.capture {
            capture.write("write_isize", &(value as i64).to_le_bytes());
        }
    }
}

pub(crate) struct RecordingSha256 {
    inner: Sha256,
    capture: Option<Capture>,
}
impl RecordingSha256 {
    pub(crate) fn new(inner: Sha256, boundary: &'static str) -> Self {
        Self {
            inner,
            capture: Capture::new(boundary),
        }
    }
    pub(crate) fn result_link(&self) -> Option<Capture> {
        self.capture.clone()
    }
    pub(crate) fn update(&mut self, bytes: impl AsRef<[u8]>) {
        if let Some(capture) = &self.capture {
            capture.write("update", bytes.as_ref());
        }
        self.inner.update(bytes);
    }
    pub(crate) fn finalize(self) -> sha2::digest::Output<Sha256> {
        let output = self.inner.finalize();
        if let Some(capture) = &self.capture {
            capture.finish("finalize", &output);
        }
        output
    }
}

pub(crate) fn begin_simhash(feature_count: usize) -> Option<Capture> {
    let capture = Capture::new("simhash")?;
    capture.frame.borrow_mut().inputs[capture.index].feature_count = Some(feature_count);
    Some(capture)
}

pub(crate) fn secret_material(bytes: &[u8]) {
    if let Some(capture) = Capture::new("secret-material") {
        capture.write("canonical-json", bytes);
    }
}

#[derive(Serialize)]
pub struct Fingerprint {
    pub sha256: String,
    pub length: usize,
}

pub fn fingerprint(bytes: &[u8]) -> Fingerprint {
    Fingerprint {
        sha256: hex::encode(Sha256::digest(bytes)),
        length: bytes.len(),
    }
}

pub fn observe_secret<P: crate::resource::PlaceholderKind>(
    payload: &crate::resource::SecretPayload<P, crate::resource::AbsoluteBindingAddress>,
) -> Result<(), crate::value::SerializationError> {
    payload.hash(None).map(|_| ())
}

/// Exercise the production snapshot binding adapter and material producer.
pub fn observe_snapshot_secret<
    P: crate::resource::PlaceholderKind + serde::de::DeserializeOwned,
>(
    payload: &crate::resource::SecretPayload<P, crate::resource::AbsoluteBindingAddress>,
) -> Result<(), crate::value_walk::WalkError> {
    crate::value_walk::observe_snapshot_secret(payload)
}

pub(crate) fn secret_producer_context(producer: SecretProducer) -> ContextGuard {
    owner_context(|frame| {
        let owner = frame
            .owner
            .clone()
            .unwrap_or(InputOwner::SecretProducer(producer));
        (owner, InputContext::SecretProducer(producer))
    })
}

/// Capture the actual address builder's association, including identifier-safe
/// map keys rendered with a dot. The facade never guesses keys from spelling.
pub(crate) fn address_key(address: &str, parent: &str, key: &str) {
    ACTIVE.with(|active| {
        if let Some(frame) = active.borrow().as_ref() {
            frame.borrow_mut().address_keys.push(AddressKey {
                address: address.to_owned(),
                parent: parent.to_owned(),
                key: key.to_owned(),
            });
        }
    });
}

/// Split only producer-observed map expansions; literal source identifiers
/// retain their exact spelling. Longer expansions win so nested keys recurse.
pub fn address_parts(address: &str) -> Vec<AddressPart> {
    fn split(address: &str, keys: &[AddressKey]) -> Vec<AddressPart> {
        let candidate = keys
            .iter()
            .filter_map(|key| {
                address
                    .match_indices(&key.address)
                    .find(|(start, _)| {
                        (*start == 0 || address.as_bytes()[start - 1] == b'.')
                            && (start + key.address.len() == address.len()
                                || matches!(
                                    address.as_bytes()[start + key.address.len()],
                                    b'.' | b'['
                                ))
                    })
                    .map(|(start, _)| (key, start))
            })
            .max_by_key(|(key, start)| (key.address.len(), std::cmp::Reverse(*start)));
        match candidate {
            Some((key, start)) => {
                let mut parts = split(&format!("{}{}", &address[..start], key.parent), keys);
                parts.push(AddressPart::MapKey(key.key.clone()));
                let suffix = &address[start + key.address.len()..];
                if !suffix.is_empty() {
                    parts.extend(split(suffix, keys));
                }
                parts
            }
            None => vec![AddressPart::Literal(address.to_owned())],
        }
    }
    ACTIVE.with(|active| {
        let active = active.borrow();
        match active.as_ref() {
            Some(frame) => split(address, &frame.borrow().address_keys),
            None => vec![AddressPart::Literal(address.to_owned())],
        }
    })
}

pub fn resource_location<S, P: ValueKind, B: BindingAddress, D>(
    resource: &Resource<S, P, B, D>,
) -> Option<&DeclarationLocation> {
    resource.declaration.location()
}

pub fn resource_key<S: AnonymousIdentityModuleSource, P: ValueKind, B: BindingAddress, D>(
    resource: &Resource<S, P, B, D>,
) -> Option<ResourceKey> {
    let declaration = resource.authored_slots.declaration_id()?.clone();
    let modules = match &resource.module_source {
        Some(source) => source.recording_module_calls()?,
        None => Vec::new(),
    };
    let loops = resource
        .loop_provenance
        .direct_origin()
        .into_iter()
        .chain(
            resource
                .loop_provenance
                .module_prefixes()
                .iter()
                .map(|prefix| prefix.origin()),
        )
        .map(|origin| {
            (
                origin.source().scheduler_identity().clone(),
                origin.kind().clone(),
            )
        })
        .collect();
    Some(ResourceKey {
        declaration,
        modules,
        loops,
    })
}

pub fn loop_system_identities(
    provenance: &crate::resource::LoopProvenance,
) -> Vec<ResourceIdentity> {
    provenance
        .direct_origin()
        .into_iter()
        .chain(
            provenance
                .module_prefixes()
                .iter()
                .map(|prefix| prefix.origin()),
        )
        .map(|origin| origin.source().scheduler_identity().clone())
        .collect()
}

#[cfg(test)]
mod tests;
