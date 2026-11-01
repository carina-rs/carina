// Regression fixture for #4185: a wasm32 crate that links the SDK without a
// provider export must still componentize.
// Any new SDK-internal wit-bindgen binding must also be reached from this
// main, or this fixture will not cover it.
fn main() {
    let _ = carina_plugin_sdk::aws_credentials::current();
}
