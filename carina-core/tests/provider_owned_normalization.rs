//! Provider-owned normalization keeps its unbound error type on native and WASM builds.
#[path = "support/provider_text.rs"]
mod text_fixtures;
use carina_core::provider::{
    BoxFuture, ProviderError, ProviderNormalizer, ProviderResult, SavedAttrs, StateTransformMap,
};
use carina_core::resource::{ProviderResource, Resource};
use text_fixtures::NativeWireFixture;
struct RawNormalizer;
impl ProviderNormalizer for RawNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _: &'a mut [ProviderResource],
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::invalid_input("invalid region")) })
    }
    fn normalize_state<'a>(
        &'a self,
        _: &'a mut StateTransformMap,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::invalid_input("invalid state region")) })
    }
    fn hydrate_read_state<'a>(
        &'a self,
        _: &'a mut StateTransformMap,
        _: &'a SavedAttrs,
    ) -> BoxFuture<'a, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn review_r2_provider_owned_desired_normalization_accepts_raw_normalizer() {
    let result: ProviderResult<_> = carina_core::executor::normalized::apply_desired_normalization(
        Resource::new("test.resource", "probe"),
        &[],
        &RawNormalizer,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().to_wire().message, "invalid region");
}
#[tokio::test]
async fn review_r2_provider_owned_state_normalization_accepts_raw_normalizer() {
    let result: ProviderResult<_> =
        carina_core::provider::normalize_provider_states(&RawNormalizer, &mut Default::default())
            .await;
    assert_eq!(
        result.unwrap_err().to_wire().message,
        "invalid state region"
    );
}
