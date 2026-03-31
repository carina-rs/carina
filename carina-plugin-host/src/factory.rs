//! ProcessProviderFactory spawns a provider process and implements ProviderFactory.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory};
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;
use carina_provider_protocol::types::ProviderInfo;

use crate::convert;
use crate::process::ProviderProcess;
use crate::provider::ProcessProvider;

pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
    name_static: &'static str,
    display_name_static: &'static str,
}

impl ProcessProviderFactory {
    /// Create a new ProcessProviderFactory by spawning the binary and querying provider_info.
    pub fn new(binary_path: PathBuf) -> Result<Self, String> {
        let mut process = ProviderProcess::spawn(&binary_path)?;

        let result: methods::ProviderInfoResult = process
            .call("provider_info", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get provider_info: {e}"))?;

        let name_static: &'static str = Box::leak(result.info.name.clone().into_boxed_str());
        let display_name_static: &'static str =
            Box::leak(result.info.display_name.clone().into_boxed_str());

        // Shut down this temporary process — a new one will be spawned for actual use
        process.shutdown();

        Ok(Self {
            binary_path,
            info: result.info,
            name_static,
            display_name_static,
        })
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
        let binary_path = self.binary_path.clone();
        let attrs = convert::core_to_proto_value_map(attributes);
        let name = self.info.name.clone();

        Box::pin(async move {
            let mut process =
                ProviderProcess::spawn(&binary_path).expect("Failed to spawn provider process");

            let params = methods::InitializeParams { attributes: attrs };
            let _result: methods::InitializeResult = process
                .call("initialize", &params)
                .expect("Failed to initialize provider");

            Box::new(ProcessProvider::new(process, name)) as Box<dyn Provider>
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        // For Phase 1, return empty — Mock provider has no schemas.
        // Full schema conversion (proto → core) will be implemented in Phase 2.
        vec![]
    }
}
