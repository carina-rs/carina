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
#[derive(Clone, Debug)]
pub struct GuestInputInventory {
    secrets: RedactionSet,
}
impl GuestInputInventory {
    pub(super) fn new(inputs: &ProviderValidationInputs) -> Self {
        Self {
            secrets: inputs.redaction_set().clone(),
        }
    }
    /// Bind stderr directly without exposing the inventory's redaction set.
    pub fn line_writer<W: std::io::Write>(&self, sink: W) -> super::RedactingLineWriter<W> {
        super::RedactingLineWriter::new(&self.secrets, sink)
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

    /// Collect the same opaque payload used by DSL secrets before projecting
    /// its wire value. This is the single plaintext credential handoff.
    pub fn handoff(
        &self,
        secret: &SecretPayload<NoPlaceholder, AbsoluteBindingAddress>,
    ) -> Result<carina_provider_protocol::types::Value, crate::protocol_convert::ProtocolConvertError>
    {
        let value = Value::Deferred(DeferredValue::Secret(secret.clone()));
        self.secrets.extend(&RedactionSet::collect([&value]));
        crate::provider_wire::project_plaintext(&value)
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
