//! Store-owned credential handoff authority for the native guest runtime.

use std::ops::{Deref, DerefMut};

use super::{GuestCredentialHandoff, GuestInputInventory};
use crate::resource::{AbsoluteBindingAddress, NoPlaceholder, SecretPayload};

/// Delegate an enclosing error's captured rendering to an existing diagnostic.
///
/// The diagnostic already owns its complete text, typed cause, retry policy,
/// and redaction inventories. This identity wrapper accepts no replacement
/// text and grants no binding or plaintext access. Core applies its ordinary
/// redaction rules when capturing it, including when a foreign error returns
/// it from `Error::source()` to avoid rendering its own inline cause twice.
/// Raw `Error` walkers cannot expose the stored text or underlying source.
///
/// ```compile_fail,E0423
/// use carina_core::provider_text::{ProviderDiagnostic, RenderedGuestDiagnostic};
/// let source = ProviderDiagnostic::new(std::io::Error::other("typed leaf"));
/// let _ = RenderedGuestDiagnostic(source);
/// ```
///
/// ```
/// use carina_core::provider_text::{ProviderDiagnostic, RenderedGuestDiagnostic};
/// let source = ProviderDiagnostic::new(std::io::Error::other("typed leaf"));
/// let _ = RenderedGuestDiagnostic::from(source);
/// ```
#[derive(Clone, Debug)]
pub struct RenderedGuestDiagnostic(pub(super) super::ProviderDiagnostic);

impl From<super::ProviderDiagnostic> for RenderedGuestDiagnostic {
    fn from(source: super::ProviderDiagnostic) -> Self {
        Self(source)
    }
}

impl std::fmt::Display for RenderedGuestDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<provider cause>")
    }
}

impl std::error::Error for RenderedGuestDiagnostic {}

/// Guest data whose append-only inventory is installed together with its host
/// state. There is no standalone constructor or conversion from an inventory.
/// Calls observe this same inventory before ingesting the guest's diagnostics.
pub struct GuestStoreData<T> {
    state: T,
    inputs: GuestInputInventory,
}

/// Owns the live store. Only borrowed store access is exposed, so callers
/// cannot use `Store::into_data` to detach its credential capability.
///
/// ```compile_fail,E0507
/// use carina_core::provider_text::GuestInputStore;
/// fn detach(store: GuestInputStore<()>) {
///     let _ = store.into_data();
/// }
/// ```
pub struct GuestInputStore<T: 'static> {
    store: wasmtime::Store<GuestStoreData<T>>,
}

impl<T> GuestStoreData<T> {
    /// Install the inventory before any credential can cross the guest wire.
    pub fn new_store(
        engine: &wasmtime::Engine,
        state: T,
        inputs: GuestInputInventory,
    ) -> GuestInputStore<T>
    where
        T: 'static,
    {
        GuestInputStore {
            store: wasmtime::Store::new(engine, Self { state, inputs }),
        }
    }

    pub fn input_inventory(&self) -> &GuestInputInventory {
        &self.inputs
    }

    /// Preserve credentials retained by the instance when adopting call inputs.
    pub fn set_input_inventory(&mut self, inputs: GuestInputInventory) {
        inputs.join(&self.inputs);
        self.inputs = inputs;
    }

    pub fn credential_handoff<'a>(
        &'a self,
        secret: &'a SecretPayload<NoPlaceholder, AbsoluteBindingAddress>,
    ) -> GuestCredentialHandoff<'a> {
        GuestCredentialHandoff {
            inputs: &self.inputs,
            secret,
        }
    }
}

impl<T: 'static> Deref for GuestInputStore<T> {
    type Target = wasmtime::Store<GuestStoreData<T>>;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl<T: 'static> DerefMut for GuestInputStore<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl<T> Deref for GuestStoreData<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl<T> DerefMut for GuestStoreData<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl<T: wasmtime_wasi::WasiView> wasmtime_wasi::WasiView for GuestStoreData<T> {
    fn ctx(&mut self) -> wasmtime_wasi::WasiCtxView<'_> {
        self.state.ctx()
    }
}

impl<T: wasmtime_wasi_http::p2::WasiHttpView> wasmtime_wasi_http::p2::WasiHttpView
    for GuestStoreData<T>
{
    fn http(&mut self) -> wasmtime_wasi_http::p2::WasiHttpCtxView<'_> {
        self.state.http()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderPlan;
    use crate::resource::{ConcreteValue, Value};

    #[test]
    fn handoff_registers_with_the_installed_store_and_publishes_to_the_call() {
        let engine = wasmtime::Engine::default();
        let session = ProviderPlan::state_only().test_session("", None, None);
        let source = session.guest_inputs();
        let store = GuestStoreData::new_store(&engine, (), session.guest_inventory());
        let call = store.data().input_inventory().observe(&source);
        let payload = Box::new(Value::Concrete(ConcreteValue::String(
            "credential-sentinel".into(),
        )))
        .into();
        {
            let receiver = ProviderPlan::state_only()
                .test_session("", None, None)
                .guest_inventory();
            let wire = receiver
                .handoff(store.data().credential_handoff(&payload))
                .unwrap();
            assert_eq!(
                wire,
                carina_provider_protocol::types::Value::String("credential-sentinel".into())
            );
        }
        let mut output = Vec::new();
        {
            let mut stderr = store.data().input_inventory().line_writer(&mut output);
            stderr.push_str("guest credential-sentinel\n").unwrap();
            stderr.finish().unwrap();
        }
        assert_eq!(output, b"guest (secret)\n");
        drop(call);
        assert_eq!(
            source
                .redaction_set()
                .redact(&super::super::ProviderText::new("credential-sentinel"))
                .as_str(),
            "(secret)"
        );
    }
}
