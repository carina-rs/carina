//! SDK-owned bindings for providers without network imports.
#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider",
    with: {
        "carina:provider/types@0.2.0": carina_plugin_bindings_diagnostics::carina::provider::types,
        "carina:provider/diagnostics@0.2.0": carina_plugin_bindings_diagnostics::carina::provider::diagnostics,
        "carina:provider/protected-material@0.2.0": carina_plugin_bindings_diagnostics::carina::provider::protected_material,
    },
    pub_export_macro: true,
    // Resolve generated helpers in this crate even when the SDK re-exports
    // the macro into a provider that has no direct binding-crate dependency.
    default_bindings_module: "$crate",
});
