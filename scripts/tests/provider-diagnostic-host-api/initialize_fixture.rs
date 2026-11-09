use carina_core::provider::{ProviderFactory, ProviderReadyConfig};
use carina_core::test_utils::provider_text::InitializeFixture;

pub fn initialize<T: ProviderFactory>(factory: &T, config: &ProviderReadyConfig) {
    let _ = factory.initialize_fixture(None, config);
    let _ = factory.initialize_normalizer_fixture(None, config);
}
