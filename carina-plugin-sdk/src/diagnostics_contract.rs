//! Distinct diagnostic API rejection probes; positive controls live beside each API.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics) { d.emit("raw"); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, value: &dyn std::fmt::Display) { d.emit(value); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, value: &dyn std::fmt::Debug) { d.emit(value); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, event: carina_plugin_sdk::protocol::diagnostics::Event) { d.emit(event); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &std::io::Error) {
//!     let _ = ProviderEvent::failure(EventLevel::Warn, "retry".into(), error);
//! }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! use aws_smithy_runtime_api::client::result::SdkError;
//! fn reject(error: &SdkError<std::io::Error>) {
//!     let _ = ProviderEvent::failure(EventLevel::Warn, "retry".into(), error);
//! }
//! ```
//!
//! ```compile_fail,E0277
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &ProviderError) { let _ = format!("{error}"); }
//! ```
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &ProviderError) { let _ = error.to_string(); }
//! ```
//!
//! ```compile_fail,E0599
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn reject(value: &ProviderDiagnostic) { let _ = value.to_string(); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::ProviderText;
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&ProviderText::new("raw"));
//! ```
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::HostText;
//! use carina_core::provider_text::present_event;
//! fn reject(text: &HostText) { let _ = present_event(text); }
//! ```
//!
//! ```compile_fail,E0308
//! use carina_core::provider::{PartialReadDiagnostic};
//! use carina_core::provider_text::Bound;
//! use carina_core::provider_text::present_event;
//! fn reject(reason: &PartialReadDiagnostic<Bound>) { let _ = present_event(reason); }
//! ```
//!
//! ```compile_fail,E0624
//! use carina_core::provider_text::{ProviderText, RedactionSet};
//! fn reject(text: ProviderText, inventory: &RedactionSet) { let _ = text.redact(inventory); }
//! ```
//!
//! ```compile_fail,E0603
//! use carina_core::provider_text::events::ProviderTextScope;
//! ```
//!
//! ```compile_fail,E0277
//! use carina_plugin_sdk::protected_material::RegisteredGithubToken;
//! fn reject(value: RegisteredGithubToken) { let _ = serde_json::to_string(&value); }
//! ```
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! let _ = ProviderEvent::panic("raw");
//! ```
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! let _ = ProviderEvent::http("raw");
//! ```

//! SDK events expose no transport extraction or inner event field.
//! ```compile_fail,E0599
//! use carina_plugin_sdk::ProviderEvent;
//! fn extract(event: ProviderEvent<'_>) { let _ = event.into_wire(); }
//! ```
//! ```compile_fail,E0616
//! use carina_plugin_sdk::ProviderEvent;
//! fn extract(event: ProviderEvent<'_>) { let _ = event.0; }
//! ```
