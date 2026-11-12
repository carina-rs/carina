//! Append-only inputs retained by a guest instance, including host credentials.
use super::RedactionSet;
use crate::provider::ProviderValidationInputs;
use crate::resource::{AbsoluteBindingAddress, DeferredValue, NoPlaceholder, SecretPayload, Value};

/// One inventory for the lifetime of a guest instance. It collects typed
/// secrets at the wire handoff and joins each checked call's input inventory.
/// It cannot mint a runtime session or display unbound provider output.
///
/// Inventories cannot be minted without core-owned inputs.
///
/// ```compile_fail,E0624
/// use carina_core::provider::{ProviderValidationInputs};
/// fn forge(inputs: &ProviderValidationInputs) {
///     let _ = carina_core::provider_text::GuestInputInventory::new(inputs);
/// }
/// ```
/// ```compile_fail,E0277
/// let _: carina_core::provider_text::GuestInputInventory = Default::default();
/// ```
///
/// Plain strings cannot be handed to a guest as credentials.
///
/// ```compile_fail,E0308
/// use carina_core::provider_text::GuestInputInventory;
/// fn handoff(inputs: &GuestInputInventory) {
///     let _ = inputs.handoff(&String::from("unprotected credential"));
/// }
/// ```
/// ```compile_fail,E0599
/// use carina_core::provider_text::GuestInputInventory;
/// fn extract(inputs: &GuestInputInventory) { let _ = inputs.redaction_set().clone(); }
/// ```
/// ```compile_fail,E0308
/// use carina_core::provider_text::{GuestInputInventory, RedactionSet};
/// fn observe(inputs: &GuestInputInventory, bare: &RedactionSet) { let _ = inputs.observe(bare); }
/// ```
///
/// An inventory alone cannot extract the plaintext of an opaque secret.
///
/// ```compile_fail,E0308
/// use carina_core::provider::ProviderValidationInputs;
/// use carina_core::resource::{AbsoluteBindingAddress, NoPlaceholder, SecretPayload};
/// fn extract(payload: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
///     let inputs = ProviderValidationInputs::for_schema("").guest_inventory();
///     let _: Result<carina_provider_protocol::types::Value, _> = inputs.handoff(payload);
/// }
/// ```
#[derive(Clone, Debug)]
pub struct GuestInputInventory {
    secrets: RedactionSet,
    diagnostics: super::ProviderDiagnosticChannel,
    _diagnostic_owner: super::DiagnosticOwner,
}
impl GuestInputInventory {
    pub(super) fn new(inputs: &ProviderValidationInputs) -> Self {
        Self {
            secrets: inputs.redaction_set().clone(),
            diagnostics: inputs.diagnostics(),
            _diagnostic_owner: inputs.diagnostic_owner(),
        }
    }
    /// Bind stderr directly without exposing the inventory's redaction set.
    pub fn line_writer<W: std::io::Write>(&self, sink: W) -> super::RedactingLineWriter<W> {
        super::RedactingLineWriter::new(&self.secrets, sink)
    }
    pub fn diagnostics(&self) -> super::ProviderDiagnosticChannel {
        self.diagnostics.clone()
    }
    pub fn validation_inputs(&self) -> ProviderValidationInputs {
        ProviderValidationInputs::from_parts(
            self.secrets.clone(),
            self.diagnostics.clone(),
            self._diagnostic_owner.clone(),
        )
    }
    host_api! {
        fn for_diagnostic_call(&self) -> (Self, super::ProviderDiagnosticCall) {
            let (diagnostics, call) = self.diagnostics.for_call();
            (
                Self {
                    secrets: self.secrets.clone(),
                    diagnostics,
                    _diagnostic_owner: self._diagnostic_owner.clone(),
                },
                call,
            )
        }
    }
    /// Preserve inputs handed to an earlier store when adopting its owner.
    pub fn join(&self, other: &Self) {
        self.secrets.extend(&other.secrets);
        other.secrets.extend(&self.secrets);
    }
    pub fn is_empty(&self) -> bool {
        self.secrets.snapshot().is_empty()
    }

    /// Decide whether another validation would exceed this instance's distinct
    /// secret-value budget, without joining its inputs into the old inventory.
    pub fn would_exceed_secret_limit(
        &self,
        inputs: &ProviderValidationInputs,
        limit: usize,
    ) -> bool {
        self.secrets
            .would_exceed_secret_limit(inputs.redaction_set(), limit)
    }

    /// Cover acquisition errors with earlier inputs without retaining a call
    /// that never entered this guest instance.
    pub fn publish_to(&self, inputs: &ProviderValidationInputs) {
        inputs.redaction_set().extend(&self.secrets);
    }

    /// Project a credential only with the capability borrowed from its live
    /// guest store. Registration always reaches that store's input inventory,
    /// even if the receiver is a separate inventory.
    pub fn handoff(
        &self,
        handoff: GuestCredentialHandoff<'_>,
    ) -> Result<carina_provider_protocol::types::Value, crate::protocol_convert::ProtocolConvertError>
    {
        self.join(handoff.inputs);
        let value = handoff.inputs.project_credential(handoff.secret);
        self.join(handoff.inputs);
        value
    }

    fn project_credential(
        &self,
        secret: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>,
    ) -> Result<carina_provider_protocol::types::Value, crate::protocol_convert::ProtocolConvertError>
    {
        let value = Value::Deferred(DeferredValue::Secret(secret.clone()));
        self.secrets.extend(&RedactionSet::collect([&value]));
        crate::provider_wire::project_plaintext(&value)
    }

    /// Inspect a credential in test fixtures without minting a live guest store.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn handoff_for_test(
        &self,
        secret: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>,
    ) -> Result<carina_provider_protocol::types::Value, crate::protocol_convert::ProtocolConvertError>
    {
        self.project_credential(secret)
    }

    /// Join only core-collected inputs. Previous calls remain covered even if
    /// the guest remembers their values. Drop publishes newly handed-off host
    /// credentials back to the caller before its error-ingestion boundary.
    pub fn observe(&self, inputs: &ProviderValidationInputs) -> GuestInputCall {
        let inputs = inputs.redaction_set();
        self.secrets.extend(inputs);
        inputs.extend(&self.secrets);
        GuestInputCall {
            inventory: self.clone(),
            inputs: inputs.clone(),
        }
    }
}

/// A credential handoff borrowed from data installed in a live guest store.
/// Its fields and constructor are private; a session's inventory cannot mint it.
///
/// ```compile_fail,E0451
/// use carina_core::provider_text::{GuestCredentialHandoff, GuestInputInventory};
/// use carina_core::resource::{AbsoluteBindingAddress, NoPlaceholder, SecretPayload};
/// fn forge(inputs: &GuestInputInventory, secret: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>) {
///     let _ = GuestCredentialHandoff { inputs, secret };
/// }
/// ```
pub struct GuestCredentialHandoff<'a> {
    pub(super) inputs: &'a GuestInputInventory,
    pub(super) secret: &'a SecretPayload<NoPlaceholder, AbsoluteBindingAddress>,
}

#[must_use]
pub struct GuestInputCall {
    inventory: GuestInputInventory,
    inputs: RedactionSet,
}
impl Drop for GuestInputCall {
    fn drop(&mut self) {
        self.inputs.extend(&self.inventory.secrets);
    }
}
