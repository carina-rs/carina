use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read STS caller identity (data source)
    ///
    /// Calls STS GetCallerIdentity and returns account_id, arn, user_id.
    /// Always succeeds regardless of identifier (STS doesn't need one).
    pub(crate) async fn read_sts_caller_identity(&self, id: &ResourceId) -> ProviderResult<State> {
        let response = self
            .sts_client
            .get_caller_identity()
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to get STS caller identity")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        let mut attributes = HashMap::new();
        if let Some(account) = response.account() {
            attributes.insert("account_id".to_string(), Value::String(account.to_string()));
        }
        if let Some(arn) = response.arn() {
            attributes.insert("arn".to_string(), Value::String(arn.to_string()));
        }
        if let Some(user_id) = response.user_id() {
            attributes.insert("user_id".to_string(), Value::String(user_id.to_string()));
        }

        Ok(State::existing(id.clone(), attributes))
    }
}
