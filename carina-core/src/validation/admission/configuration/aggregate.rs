use super::*;

/// Exact declaration placements and their immutable canonical payload.
/// Every payload field is derived from a placed value.
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
    declaration: Arc<ConfigurationDeclaration>,
    instance: K::Instance,
}

impl<K: ConfigurationKind> AdmittedConfiguration<K> {
    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn declaration_schema(&self) -> &ValidatedConfigSchema {
        &self.declaration.schema
    }

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
}
impl AdmittedConfiguration<kind::BackendAttribute> {
    pub fn backend_kind(&self) -> backend::BackendKind {
        self.instance
    }
}

pub(super) struct ConfigurationBuilder<K: ConfigurationKind> {
    fields: ConfigurationAttributes<PlacedValue<K>>,
    declaration: Arc<ConfigurationDeclaration>,
    instance: K::Instance,
}
impl<K: ConfigurationKind> ConfigurationBuilder<K> {
    pub(super) fn new(declaration: Arc<ConfigurationDeclaration>, instance: K::Instance) -> Self {
        Self {
            fields: Default::default(),
            declaration,
            instance,
        }
    }
    pub(super) fn place(&mut self, key: crate::parser::AttributeKey, placed: PlacedValue<K>) {
        self.fields.insert(key, placed);
    }
    fn values(&self) -> ConfigurationAttributes<Value> {
        self.fields
            .iter()
            .map(|(key, placed)| (key.clone(), placed.value().clone()))
            .collect()
    }
    fn finish(self, payload: K::Payload) -> AdmittedConfiguration<K> {
        AdmittedConfiguration {
            payload,
            fields: self.fields,
            declaration: self.declaration,
            instance: self.instance,
        }
    }
}
impl ConfigurationBuilder<kind::ProviderConfigAttribute> {
    fn provider_payload(
        &self,
    ) -> Result<WireReady<ConfigurationAttributes<Value>>, crate::provider::ProviderWireError> {
        WireReady::try_new(
            self.values(),
            ProviderWireTarget::Configuration {
                provider: self.instance.provider().to_owned(),
            },
        )
    }
    pub(super) fn analyze_payload(
        self,
    ) -> Result<WireReady<ConfigurationAttributes<Value>>, crate::provider::ProviderWireError> {
        self.provider_payload()
    }
    pub(super) fn admit_payload(
        self,
    ) -> Result<
        AdmittedConfiguration<kind::ProviderConfigAttribute>,
        crate::provider::ProviderWireError,
    > {
        let payload = self.provider_payload()?;
        Ok(self.finish(payload))
    }
}

impl ConfigurationBuilder<kind::BackendAttribute> {
    pub(super) fn admit_payload(self) -> AdmittedConfiguration<kind::BackendAttribute> {
        let payload = self.values();
        self.finish(payload)
    }
}
