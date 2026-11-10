//! Shared imports-only diagnostic and confidential-registration bindings.
#![cfg(target_arch = "wasm32")]
wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    inline: r#"
        package carina:plugin-sdk;
        world diagnostic-client {
            import carina:provider/diagnostics@0.3.0;
            import carina:provider/protected-material@0.3.0;
        }
    "#,
    world: "carina:plugin-sdk/diagnostic-client",
    generate_all,
    generate_unused_types: true,
    type_section_suffix: "carina-plugin-sdk-diagnostics",
});
