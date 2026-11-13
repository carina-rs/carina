use super::*;

/// Exact declaration placements and explicitly opaque factory-owned values.
/// The canonical payload is built only while those parts are assembled.
///
/// ```compile_fail
/// use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::ProviderConfigAttribute};
/// fn bypass(config: &AdmittedConfiguration<ProviderConfigAttribute>) {
///     let _ = &config.payload; // forbidden: the existing payload field is private
/// }
/// ```
/// ```
/// # use carina_core::validation::admission::{configuration::AdmittedConfiguration, kind::ProviderConfigAttribute};
/// # fn bypass(config: &AdmittedConfiguration<ProviderConfigAttribute>) {
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct AdmittedConfiguration<K: ConfigurationKind> {
    payload: K::Payload,
    fields: ConfigurationAttributes<PlacedValue<K>>,
    opaque_passthrough: ConfigurationAttributes<Value>,
    declaration: Arc<ConfigurationDeclaration>,
    instance: K::Instance,
}

impl<K: ConfigurationKind> AdmittedConfiguration<K> {
    pub fn attributes(&self) -> &ConfigurationAttributes<Value> {
        K::attributes(&self.payload)
    }
    #[cfg(test)]
    pub(super) fn placements(&self) -> &ConfigurationAttributes<PlacedValue<K>> {
        &self.fields
    }
    pub(super) fn field(&self, name: &str) -> Option<&PlacedValue<K>> {
        self.declaration.schema.parts().get(name)?;
        self.fields.get(name)
    }
}
impl AdmittedConfiguration<kind::ProviderConfigAttribute> {
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.payload.wire()
    }
    pub(super) fn canonical_payload(&self) -> WireReady<ConfigurationAttributes<Value>> {
        self.payload.clone()
    }
    pub fn instance(&self) -> &ProviderInstanceKey {
        &self.instance
    }
    pub fn opaque_passthrough(&self) -> &ConfigurationAttributes<Value> {
        &self.opaque_passthrough
    }
}
impl AdmittedConfiguration<kind::BackendAttribute> {
    pub fn backend_kind(&self) -> backend::BackendKind {
        self.instance
    }
}

pub(super) struct ConfigurationBuilder<K: ConfigurationKind> {
    values: ConfigurationAttributes<Value>,
    fields: ConfigurationAttributes<PlacedValue<K>>,
    opaque_passthrough: ConfigurationAttributes<Value>,
    declaration: Arc<ConfigurationDeclaration>,
    instance: K::Instance,
}
impl<K: ConfigurationKind> ConfigurationBuilder<K> {
    pub(super) fn new(declaration: Arc<ConfigurationDeclaration>, instance: K::Instance) -> Self {
        Self {
            values: Default::default(),
            fields: Default::default(),
            opaque_passthrough: Default::default(),
            declaration,
            instance,
        }
    }
    pub(super) fn place(&mut self, key: crate::parser::AttributeKey, placed: PlacedValue<K>) {
        self.values.insert(key.clone(), placed.value().clone());
        self.fields.insert(key, placed);
    }
    fn finish(self, payload: K::Payload) -> AdmittedConfiguration<K> {
        AdmittedConfiguration {
            payload,
            fields: self.fields,
            opaque_passthrough: self.opaque_passthrough,
            declaration: self.declaration,
            instance: self.instance,
        }
    }
}
impl ConfigurationBuilder<kind::ProviderConfigAttribute> {
    pub(super) fn passthrough(&mut self, key: crate::parser::AttributeKey, value: Value) {
        self.values.insert(key.clone(), value.clone());
        self.opaque_passthrough.insert(key, value);
    }
    fn provider_payload(
        &mut self,
    ) -> Result<WireReady<ConfigurationAttributes<Value>>, crate::provider::ProviderWireError> {
        WireReady::try_new(
            std::mem::take(&mut self.values),
            ProviderWireTarget::Configuration {
                provider: self.instance.provider().to_owned(),
            },
        )
    }
    pub(super) fn analyze_payload(
        mut self,
    ) -> Result<WireReady<ConfigurationAttributes<Value>>, crate::provider::ProviderWireError> {
        self.provider_payload()
    }
    pub(super) fn admit_payload(
        mut self,
    ) -> Result<
        AdmittedConfiguration<kind::ProviderConfigAttribute>,
        crate::provider::ProviderWireError,
    > {
        let payload = self.provider_payload()?;
        Ok(self.finish(payload))
    }
}

impl ConfigurationBuilder<kind::BackendAttribute> {
    pub(super) fn admit_payload(mut self) -> AdmittedConfiguration<kind::BackendAttribute> {
        let payload = std::mem::take(&mut self.values);
        self.finish(payload)
    }
}
