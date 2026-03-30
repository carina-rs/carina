//! AWS Provider factory implementation

use std::collections::HashMap;

use carina_core::provider::{BoxFuture, ProviderFactory, ProviderNormalizer};
use carina_core::resource::Value;

use crate::AwsProvider;
use crate::normalizer::AwsNormalizer;

/// Factory for creating and configuring the AWS Provider
pub struct AwsProviderFactory;

impl ProviderFactory for AwsProviderFactory {
    fn name(&self) -> &str {
        "aws"
    }

    fn display_name(&self) -> &str {
        "AWS provider"
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let region_type = crate::schemas::types::aws_region();
        if let Some(region_value) = attributes.get("region") {
            region_type
                .validate(region_value)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            return carina_core::utils::convert_region_value(region);
        }
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn carina_core::provider::Provider>> {
        let region = self.extract_region(attributes);
        Box::pin(async move {
            Box::new(AwsProvider::new(&region).await) as Box<dyn carina_core::provider::Provider>
        })
    }

    fn create_normalizer(
        &self,
        _attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Some(Box::new(AwsNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        crate::schemas::all_schemas()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        carina_aws_types::region_completions("aws")
    }

    fn get_enum_alias_reverse(
        &self,
        resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<&'static str> {
        crate::schemas::generated::get_enum_alias_reverse(resource_type, attr_name, value)
    }
}
