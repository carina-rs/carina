//! SDK-owned bindings for providers with declared HTTP egress.
#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider-with-http",
    pub_export_macro: true,
    default_bindings_module: "$crate",
    with: {
        "carina:provider/types@0.3.0": carina_plugin_bindings_diagnostics::carina::provider::types,
        "carina:provider/diagnostics@0.3.0": carina_plugin_bindings_diagnostics::carina::provider::diagnostics,
        "carina:provider/protected-material@0.3.0": carina_plugin_bindings_diagnostics::carina::provider::protected_material,
        "wasi:io/poll@0.2.6": wasi::io::poll,
        "wasi:io/error@0.2.6": wasi::io::error,
        "wasi:io/streams@0.2.6": wasi::io::streams,
        "wasi:clocks/monotonic-clock@0.2.6": wasi::clocks::monotonic_clock,
        "wasi:http/types@0.2.6": wasi::http::types,
        "wasi:http/outgoing-handler@0.2.6": wasi::http::outgoing_handler,
    },
});
