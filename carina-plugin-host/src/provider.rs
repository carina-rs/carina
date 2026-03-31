//! ProcessProvider wraps a ProviderProcess and implements the carina-core Provider trait.

use std::sync::Mutex;

use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State};
use carina_provider_protocol::methods;

use crate::convert;
use crate::process::ProviderProcess;

pub struct ProcessProvider {
    process: Mutex<ProviderProcess>,
    name: &'static str,
}

impl ProcessProvider {
    pub fn new(process: ProviderProcess, name: String) -> Self {
        let name_static: &'static str = Box::leak(name.into_boxed_str());
        Self {
            process: Mutex::new(process),
            name: name_static,
        }
    }
}

impl Provider for ProcessProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::ReadParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.map(String::from),
        };
        Box::pin(async move {
            let mut process = self
                .process
                .lock()
                .map_err(|e| ProviderError::new(format!("Process lock poisoned: {e}")))?;
            let result: methods::ReadResult =
                process.call("read", &params).map_err(ProviderError::new)?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::CreateParams {
            resource: convert::core_to_proto_resource(resource),
        };
        Box::pin(async move {
            let mut process = self
                .process
                .lock()
                .map_err(|e| ProviderError::new(format!("Process lock poisoned: {e}")))?;
            let result: methods::CreateResult = process
                .call("create", &params)
                .map_err(ProviderError::new)?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::UpdateParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.to_string(),
            from: convert::core_to_proto_state(from),
            to: convert::core_to_proto_resource(to),
        };
        Box::pin(async move {
            let mut process = self
                .process
                .lock()
                .map_err(|e| ProviderError::new(format!("Process lock poisoned: {e}")))?;
            let result: methods::UpdateResult = process
                .call("update", &params)
                .map_err(ProviderError::new)?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let params = methods::DeleteParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.to_string(),
            lifecycle: convert::core_to_proto_lifecycle(lifecycle),
        };
        Box::pin(async move {
            let mut process = self
                .process
                .lock()
                .map_err(|e| ProviderError::new(format!("Process lock poisoned: {e}")))?;
            let _result: methods::DeleteResult = process
                .call("delete", &params)
                .map_err(ProviderError::new)?;
            Ok(())
        })
    }
}
