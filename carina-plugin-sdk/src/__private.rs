//! Public paths required by export_provider! in downstream crates.
//!
//! Re-export bridge types, the Guest trait and the export macro.
//! Generated namespaces, credentials imports and ABI helpers stay behind the
//! binding crates' macro definition paths rather than becoming SDK modules.

macro_rules! provider_world {
    ($world:ident, $bindings:ident) => {
        pub mod $world {
            pub use $bindings::carina::provider::types::*;
            pub use $bindings::export;
            pub use $bindings::exports::carina::provider::provider::{
                DesiredResource, Guest, PlanOp,
            };
        }
    };
}

provider_world!(provider, carina_plugin_bindings_provider);
provider_world!(http, carina_plugin_bindings_http);
provider_world!(aws, carina_plugin_bindings_aws);
