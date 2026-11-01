//! SDK-owned bindings for AWS-kind providers with host-resolved credentials.
#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider-aws",
    pub_export_macro: true,
    default_bindings_module: "$crate",
    with: {
        "carina:provider/aws-credentials@0.1.0": carina_plugin_bindings_aws_credentials::carina::provider::aws_credentials,
        "wasi:io/poll@0.2.6": wasi::io::poll,
        "wasi:io/error@0.2.6": wasi::io::error,
        "wasi:io/streams@0.2.6": wasi::io::streams,
        "wasi:clocks/monotonic-clock@0.2.6": wasi::clocks::monotonic_clock,
        "wasi:http/types@0.2.6": wasi::http::types,
        "wasi:http/outgoing-handler@0.2.6": wasi::http::outgoing_handler,
    },
});
