//! Diagnostic API probes: each forbidden change has a compiling twin.
//!
//! Owned strings are not events.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics) { d.emit(String::from("raw")); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Borrowed strings are not events.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics) { d.emit("raw"); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Format arguments are not events.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics) { d.emit(format_args!("raw {}", 1)); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Arbitrary Display is not an event.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, value: &dyn std::fmt::Display) { d.emit(value); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Arbitrary Debug is not an event.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, value: &dyn std::fmt::Debug) { d.emit(value); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Wire records cannot be used as an authoring bypass.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(d: &ProviderDiagnostics, event: carina_plugin_sdk::protocol::diagnostics::Event) { d.emit(event); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Failure events borrow the typed provider error.
//!
//! ```compile_fail,E0308
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &std::io::Error) {
//!     let _ = ProviderEvent::failure(EventLevel::Warn, "retry".into(), error);
//! }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Raw SDK errors cannot become failure events.
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
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Unbound failures have no text extraction.
//!
//! ```compile_fail,E0277
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &ProviderError) { let _ = format!("{error}"); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Unbound failures have no text extraction.
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn reject(error: &ProviderError) { let _ = error.to_string(); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! Unbound diagnostics have no to_string.
//!
//! ```compile_fail,E0599
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn reject(value: &ProviderDiagnostic) { let _ = value.to_string(); }
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn accept(d: &ProviderDiagnostics, value: &ProviderDiagnostic) {
//!     d.emit(ProviderEvent::diagnostic(EventLevel::Warn, "handled".into(), value));
//! }
//! ```
//!
//! Unbound text cannot enter the event presenter.
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::ProviderText;
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&ProviderText::new("raw"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! Unbound diagnostics cannot enter the event presenter.
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::{ProviderText, ProviderDiagnostic};
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&ProviderDiagnostic::message(ProviderText::new("raw")));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! Generic Bound errors cannot enter the event presenter.
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::{ErrorText, Bound};
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&ErrorText::<Bound>::new("raw"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! Host text cannot enter the event presenter.
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::HostText;
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&HostText::from(String::from("raw")));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! String partial reasons cannot enter the event presenter.
//!
//! ```compile_fail,E0308
//! use carina_core::provider::{PartialReadDiagnostic};
//! use carina_core::provider_text::Bound;
//! use carina_core::provider_text::present_event;
//! let _ = present_event(&PartialReadDiagnostic::<Bound>::new(String::from("raw"), vec!["field".into()].try_into().unwrap()));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! No public binding method exists.
//!
//! ```compile_fail,E0624
//! use carina_core::provider_text::{ProviderText, RedactionSet};
//! fn reject(text: ProviderText, inventory: &RedactionSet) { let _ = text.redact(inventory); }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! Binding scope is private.
//!
//! ```compile_fail,E0603
//! use carina_core::provider_text::events::ProviderTextScope;
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! Bound events cannot be forged.
//!
//! ```compile_fail,E0451
//! use carina_core::provider_text::{BoundProviderEvent, EventLevel};
//! let _ = BoundProviderEvent { severity: EventLevel::Warn, segments: vec![], failure: None, diagnostic: None, secrets: todo!() };
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_core::provider_text::present_event;
//! let _session = ProviderPlan::state_only().session("probe", None, None)
//!     .with_diagnostic_sink(|event| { let _ = present_event(&event).to_string(); });
//! ```
//!
//! RegisteredAwsSession is minted only after acknowledgement.
//!
//! ```compile_fail,E0451
//! use carina_plugin_sdk::protected_material::RegisteredAwsSession;
//! let _ = RegisteredAwsSession { material: todo!(), diagnostics: todo!() };
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! use carina_plugin_sdk::protected_material::AwsSessionMaterial;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.register_aws_session(AwsSessionMaterial::new("access".into(), "secret".into(), None)).unwrap();
//! ```
//!
//! RegisteredGithubToken is minted only after acknowledgement.
//!
//! ```compile_fail,E0451
//! use carina_plugin_sdk::protected_material::RegisteredGithubToken;
//! let _ = RegisteredGithubToken { token: String::new(), diagnostics: todo!() };
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! use carina_plugin_sdk::protected_material::GithubTokenMaterial;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.register_github_token(GithubTokenMaterial::new("secret".into())).unwrap();
//! ```
//!
//! RegisteredSignedMaterial is minted only after acknowledgement.
//!
//! ```compile_fail,E0451
//! use carina_plugin_sdk::protected_material::RegisteredSignedMaterial;
//! let _ = RegisteredSignedMaterial { diagnostics: todo!() };
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! use carina_plugin_sdk::protected_material::SigningMaterial;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.register_signing_material(SigningMaterial::new("authorization".into(), None, vec![])).unwrap();
//! ```
//!
//! RegisteredRequest is minted only after acknowledgement.
//!
//! ```compile_fail,E0451
//! use carina_plugin_sdk::protected_material::RegisteredRequest;
//! let _ = RegisteredRequest { request: todo!(), registration: todo!() };
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.prepare_request(http::Request::new(Vec::new())).unwrap();
//! ```
//!
//! Registration carriers expose no plaintext.
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::protected_material::RegisteredAwsSession;
//! fn reject(value: RegisteredAwsSession) { let _ = value.secret_access_key(); }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.prepare_request(http::Request::new(Vec::new())).unwrap();
//! ```
//!
//! Registration carriers expose no serialization.
//!
//! ```compile_fail,E0277
//! use carina_plugin_sdk::protected_material::RegisteredGithubToken;
//! fn reject(value: RegisteredGithubToken) { let _ = serde_json::to_string(&value); }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderPlan;
//! use carina_plugin_sdk::ProviderDiagnostics;
//! let session = ProviderPlan::state_only().session("probe", None, None);
//! let diagnostics = ProviderDiagnostics::from_session(&session);
//! let _registered = diagnostics.prepare_request(http::Request::new(Vec::new())).unwrap();
//! ```
//!
//! Panic adapters are SDK-internal.
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! let _ = ProviderEvent::panic("raw");
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
//! HTTP adapters are SDK-internal.
//!
//! ```compile_fail,E0599
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! let _ = ProviderEvent::http("raw");
//! ```
//!
//! ```
//! use carina_plugin_sdk::{ProviderDiagnostics, ProviderEvent, EventLevel};
//! use carina_core::provider::ProviderError;
//! fn accept(d: &ProviderDiagnostics, error: &ProviderError) {
//!     d.emit(ProviderEvent::failure(EventLevel::Warn, "handled".into(), error));
//! }
//! ```
//!
