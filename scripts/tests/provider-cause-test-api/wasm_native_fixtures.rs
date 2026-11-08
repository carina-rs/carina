use carina_plugin_sdk::types::ProviderError;

pub fn native_fixture(error: ProviderError) {
    let _ = carina_plugin_sdk::core::test_utils::provider_text::NativeWireFixture::to_wire(&error);
}

pub fn native_round_trip_fixture(error: ProviderError) {
    let _ =
        carina_plugin_sdk::core::test_utils::provider_text::NativeWireFixture::round_trip(&error);
}
