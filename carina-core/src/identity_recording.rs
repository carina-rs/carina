//! Opt-in observations for the offline identity probe. Compiled only with
//! `test-utils`; an idle recorder neither collects nor allocates hash inputs.
//!
//! These records include secret plaintext. They belong only in explicit test
//! artifacts, never diagnostics, state, or normal command output.

use std::cell::RefCell;
use std::hash::Hasher;
use std::rc::Rc;

use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HashWrite {
    pub operation: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HashInput {
    pub boundary: &'static str,
    pub context: Vec<String>,
    /// Keep individual calls, including empty writes and string terminators.
    pub writes: Vec<HashWrite>,
    pub output: Option<Vec<u8>>,
}

#[derive(Default)]
struct Frame {
    context: Vec<String>,
    inputs: Vec<HashInput>,
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

/// Record only the synchronous work in this scope on this thread. Nested
/// recorders are independent; unwinding restores the preceding recorder.
pub fn record<R>(run: impl FnOnce() -> R) -> (R, Vec<HashInput>) {
    let frame = Rc::new(RefCell::new(Frame::default()));
    let previous = ACTIVE.with(|active| active.replace(Some(frame.clone())));
    let guard = RecorderGuard { previous };
    let result = run();
    drop(guard);
    let inputs = std::mem::take(&mut frame.borrow_mut().inputs);
    (result, inputs)
}

/// A context guard cannot move to another thread (it owns an `Rc`).
pub struct ContextGuard(Option<Rc<RefCell<Frame>>>);

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if let Some(frame) = &self.0 {
            frame.borrow_mut().context.pop();
        }
    }
}

pub fn context(label: impl FnOnce() -> String) -> ContextGuard {
    let frame = ACTIVE.with(|active| active.borrow().clone());
    if let Some(frame) = &frame {
        frame.borrow_mut().context.push(label());
    }
    ContextGuard(frame)
}

struct Capture {
    frame: Rc<RefCell<Frame>>,
    input: HashInput,
}

impl Capture {
    fn new(boundary: &'static str) -> Option<Self> {
        ACTIVE.with(|active| {
            let frame = active.borrow().clone()?;
            let context = frame.borrow().context.clone();
            Some(Self {
                frame,
                input: HashInput {
                    boundary,
                    context,
                    writes: Vec::new(),
                    output: None,
                },
            })
        })
    }

    fn write(&mut self, operation: &'static str, bytes: &[u8]) {
        self.input.writes.push(HashWrite {
            operation,
            bytes: bytes.to_vec(),
        });
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.frame.borrow_mut().inputs.push(self.input.clone());
    }
}

/// Forward each operation to the original hasher, without coalescing or
/// replaying bytes. In particular, forward specialized integer writes too.
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
            if let Some(capture) = &mut self.capture {
                capture.write(stringify!($method), &value.to_ne_bytes());
            }
        }
    )*};
}

impl<H: Hasher> Hasher for RecordingHasher<H> {
    fn finish(&self) -> u64 {
        self.inner.finish()
    }

    fn write(&mut self, bytes: &[u8]) {
        self.inner.write(bytes);
        if let Some(capture) = &mut self.capture {
            capture.write("write", bytes);
        }
    }

    integer_writes! {
        write_u8(u8), write_u16(u16), write_u32(u32), write_u64(u64),
        write_u128(u128), write_usize(usize),
        write_i8(i8), write_i16(i16), write_i32(i32), write_i64(i64),
        write_i128(i128), write_isize(isize),
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

    pub(crate) fn update(&mut self, bytes: impl AsRef<[u8]>) {
        if let Some(capture) = &mut self.capture {
            capture.write("update", bytes.as_ref());
        }
        self.inner.update(bytes);
    }

    pub(crate) fn finalize(mut self) -> sha2::digest::Output<Sha256> {
        let output = self.inner.finalize();
        if let Some(capture) = &mut self.capture {
            capture.input.output = Some(output.to_vec());
        }
        output
    }
}

pub(crate) fn secret_material(bytes: &[u8]) {
    if let Some(mut capture) = Capture::new("secret-material") {
        capture.write("canonical-json", bytes);
    }
}

/// Exercise the production encoder through the protected payload boundary.
/// The bytes are observed by the recorder, not reconstructed by the probe.
pub fn observe_secret<P: crate::resource::PlaceholderKind>(
    payload: &crate::resource::SecretPayload<P, crate::resource::AbsoluteBindingAddress>,
) -> Result<(), crate::value::SerializationError> {
    payload.try_inspect(|value| crate::value::canonical_secret_hash_material(value).map(|_| ()))
}

pub fn resource_location<
    S,
    P: crate::resource::ValueKind,
    B: crate::resource::BindingAddress,
    D,
>(
    resource: &crate::resource::Resource<S, P, B, D>,
) -> Option<&crate::parser::DeclarationLocation> {
    resource.declaration.location()
}

pub fn loop_system_identities(
    provenance: &crate::resource::LoopProvenance,
) -> Vec<crate::resource::ResourceIdentity> {
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

pub fn module_context_key(id: &crate::parser::ModuleCallId) -> String {
    let location = id.declaration_location();
    let mut key = format!(
        "module:{}:{}",
        location.file().path().display(),
        location.span().start_byte()
    );
    match id.expansion() {
        crate::parser::CallExpansion::Direct => {}
        crate::parser::CallExpansion::Expanded(expansion) => {
            key.push(':');
            key.push_str(expansion.as_str());
        }
    }
    key
}

#[cfg(test)]
mod tests;
