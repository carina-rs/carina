//! ProcessProviderFactory spawns a provider process and implements ProviderFactory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderNormalizer};
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;
use carina_provider_protocol::types::ProviderInfo;

use crate::convert;
use crate::normalizer::ProcessProviderNormalizer;
use crate::process::ProviderProcess;
use crate::provider::ProcessProvider;

pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
    schemas: Vec<ResourceSchema>,
    name_static: &'static str,
    display_name_static: &'static str,
    capabilities: Vec<String>,
}

impl ProcessProviderFactory {
    pub fn new(binary_path: PathBuf) -> Result<Self, String> {
        let mut process = ProviderProcess::spawn(&binary_path)?;

        let info_result: methods::ProviderInfoResult = process
            .call("provider_info", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get provider_info: {e}"))?;

        let schemas_result: methods::SchemasResult = process
            .call("schemas", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get schemas: {e}"))?;

        let schemas: Vec<ResourceSchema> = schemas_result
            .schemas
            .iter()
            .map(convert::proto_to_core_schema)
            .collect();

        let name_static: &'static str = Box::leak(info_result.info.name.clone().into_boxed_str());
        let display_name_static: &'static str =
            Box::leak(info_result.info.display_name.clone().into_boxed_str());
        let capabilities = info_result.info.capabilities.clone();

        process.shutdown();

        Ok(Self {
            binary_path,
            info: info_result.info,
            schemas,
            name_static,
            display_name_static,
            capabilities,
        })
    }

    fn spawn_and_initialize(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<Arc<Mutex<ProviderProcess>>, String> {
        let mut process = ProviderProcess::spawn(&self.binary_path)?;
        let attrs = convert::core_to_proto_value_map(attributes);
        let params = methods::InitializeParams { attributes: attrs };
        let _result: methods::InitializeResult = process
            .call("initialize", &params)
            .map_err(|e| format!("Failed to initialize provider: {e}"))?;
        Ok(Arc::new(Mutex::new(process)))
    }
}

impl ProviderFactory for ProcessProviderFactory {
    fn name(&self) -> &str {
        self.name_static
    }

    fn display_name(&self) -> &str {
        self.display_name_static
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let mut process = ProviderProcess::spawn(&self.binary_path)?;
        let params = methods::ValidateConfigParams {
            attributes: convert::core_to_proto_value_map(attributes),
        };
        let result: methods::ValidateConfigResult = process.call("validate_config", &params)?;
        process.shutdown();
        match result.error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            carina_core::utils::convert_region_value(region)
        } else {
            "ap-northeast-1".to_string()
        }
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let attrs = attributes.clone();
        let name = self.info.name.clone();
        Box::pin(async move {
            let process = self
                .spawn_and_initialize(&attrs)
                .expect("Failed to spawn provider process");
            Box::new(ProcessProvider::new(process, name)) as Box<dyn Provider>
        })
    }

    fn create_normalizer(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        let attrs = attributes.clone();
        let capabilities = self.capabilities.clone();
        Box::pin(async move {
            match self.spawn_and_initialize(&attrs) {
                Ok(process) => Some(
                    Box::new(ProcessProviderNormalizer::new(process, capabilities))
                        as Box<dyn ProviderNormalizer>,
                ),
                Err(e) => {
                    log::error!("Failed to spawn normalizer process: {e}");
                    None
                }
            }
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        self.schemas.clone()
    }
}
