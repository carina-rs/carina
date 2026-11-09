// Regression fixture for #4185: a wasm32 crate that links the SDK without a
// provider export must still componentize.
// Any new SDK-internal wit-bindgen binding must also be reached from this
// main, or this fixture will not cover it.
fn main() {
    let _ = carina_plugin_sdk::aws_credentials::current();
    #[cfg(target_arch = "wasm32")]
    {
        let diagnostic = carina_plugin_sdk::core::provider_text::ProviderDiagnostic::message(
            "SDK imports-only fixture".into(),
        );
        carina_plugin_sdk::ProviderDiagnostics::guest().emit(
            carina_plugin_sdk::ProviderEvent::diagnostic(
                carina_plugin_sdk::EventLevel::Debug,
                "componentization".into(),
                &diagnostic,
            ),
        );
    }
}
