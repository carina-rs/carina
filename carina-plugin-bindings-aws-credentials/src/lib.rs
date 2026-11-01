//! Shared imports-only credentials bindings for the SDK and AWS export world.
#![cfg(target_arch = "wasm32")]

wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    // Calling the SDK credentials API must also link without export_provider!.
    inline: r#"
        package carina:plugin-sdk;
        world aws-credentials-client {
            import carina:provider/aws-credentials@0.1.0;
        }
    "#,
    world: "carina:plugin-sdk/aws-credentials-client",
    with: {
        "carina:provider/aws-credentials@0.1.0": generate,
    },
    // LLD concatenates custom sections with the same name. Keep the
    // imports-only metadata separate from the provider's export metadata.
    type_section_suffix: "carina-plugin-sdk-aws-credentials",
});
