//! Public paths required by export_provider! in downstream crates.
//!
//! Re-export individual bridge types, the Guest trait and the export macro.
//! Generated namespaces, credentials imports and ABI helpers stay behind the
//! binding crates' macro definition paths rather than becoming SDK modules.

macro_rules! provider_world {
    ($world:ident, $bindings:ident) => {
        pub mod $world {
            pub use $bindings::carina::provider::types::{
                AppliedPersistedState, AppliedState, AttributeMatchPattern, BindingPattern,
                CreateInFlight, CreatePartialSuccess, CreateProgress, CreateRequest, CreateStep,
                CreateVerdict, CreatedFailure, DeleteOutcome, DeleteRequest, ErrorDetail,
                InitializeResult, Observation, PartialReadDiagnostic, PatchOp, PendingCreate,
                PendingDelete, PendingOperation, PendingPhase, PendingUpdate, PersistedState,
                PersistedValue, PollFinality, PollRequest, PollVerdict, ProviderError, ReadOutcome,
                ReadRequest, ReferenceScope, ReferenceScopeValues, ResourceDef, ResourceId,
                ResumeInFlight, ResumePending, ResumeStep, ResumeVerdict, ResumedFailure,
                SentAttributes, State, TypeIdentity, UpdateOutcome, UpdatePartialSuccess,
                UpdateRequest, Value,
            };
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
