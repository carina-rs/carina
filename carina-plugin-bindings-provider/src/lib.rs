//! SDK-owned bindings for providers without network imports.
#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider",
    pub_export_macro: true,
    // Resolve generated helpers in this crate even when the SDK re-exports
    // the macro into a provider that has no direct binding-crate dependency.
    default_bindings_module: "$crate",
});
