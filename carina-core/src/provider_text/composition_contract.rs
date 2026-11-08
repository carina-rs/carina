//! Composition API admission rules, with one forbidden change per compiling twin.
//!
//! invalid_input requires opaque owned runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::invalid_input_caused_by(String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::invalid_input_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! invalid_input requires opaque borrowed runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::invalid_input_caused_by(&String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::invalid_input_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! api_error requires opaque owned runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::api_error_caused_by(String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::api_error_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! api_error requires opaque borrowed runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::api_error_caused_by(&String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::api_error_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! not_found requires opaque owned runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::not_found_caused_by(String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::not_found_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! not_found requires opaque borrowed runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::not_found_caused_by(&String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::not_found_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! timeout requires opaque owned runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::timeout_caused_by(String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::timeout_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! timeout requires opaque borrowed runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::timeout_caused_by(&String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::timeout_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! internal requires opaque owned runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal_caused_by(String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! internal requires opaque borrowed runtime context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal_caused_by(&String::from("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal_caused_by(ProviderText::new("runtime"), std::io::Error::other("leaf"));
//! ```
//!
//! Errors cannot be projected into text.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderError) {
//!     let _: ProviderText = value.into();
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderText) {
//!     let _: ProviderText = value.into();
//! }
//! ```
//!
//! Diagnostics cannot be projected into text.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderDiagnostic) {
//!     let _: ProviderText = value.into();
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderText) {
//!     let _: ProviderText = value.into();
//! }
//! ```
//!
//! Errors cannot become context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderError) {
//!     let _: ProviderContext = value.into();
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderText) {
//!     let _: ProviderContext = value.into();
//! }
//! ```
//!
//! Diagnostics cannot become context.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderDiagnostic) {
//!     let _: ProviderContext = value.into();
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic, ProviderText};
//! fn convert(value: ProviderText) {
//!     let _: ProviderContext = value.into();
//! }
//! ```
//!
//! Captured diagnostic text cannot be replaced.
//!
//! ```compile_fail,E0599
//! use carina_core::provider_text::{ProviderDiagnostic, ProviderText};
//! let _ = ProviderDiagnostic::from_text(std::io::Error::other("leaf"), ProviderText::new("replacement"));
//! ```
//!
//! ```
//! use carina_core::provider_text::{ProviderDiagnostic, ProviderText};
//! let _ = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! ```
//!
//! Message-only kind constructors accept opaque text directly.
//!
//! ```compile_fail,E0599
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::from_text(ProviderErrorKind::Internal, ProviderText::new("message"));
//! ```
//!
//! ```
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal(ProviderText::new("message"));
//! ```
//!
//! Failure bodies stay private.
//!
//! ```compile_fail,E0603
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::Unbound;
//! let _ = carina_core::provider::FailureBody::<Unbound>::Message("message".into());
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::Unbound;
//! let _ = ProviderError::internal("message");
//! ```
//!
//! The opaque adapter stays private.
//!
//! ```compile_fail,E0603
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! let _ = carina_core::provider_text::OpaqueProviderFailure(ProviderError::internal("leaf"));
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! let _ = ProviderDiagnostic::new(ProviderError::internal("leaf"));
//! ```
//!
//! Unbound ProviderError is not a local Error source.
//!
//! ```compile_fail,E0599
//! use carina_core::provider::{HostError, ProviderError};
//! use carina_core::provider_text::{HostDiagnostic, ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")] // error-chain: caller-renders-sources
//! struct Wrapper { #[source] source: ProviderError }
//! ```
//!
//! ```
//! use carina_core::provider::{HostError, ProviderError};
//! use carina_core::provider_text::{HostDiagnostic, ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")] // error-chain: caller-renders-sources
//! struct Wrapper { #[source] source: HostError }
//! ```
//!
//! Unbound ProviderDiagnostic is not a local Error source.
//!
//! ```compile_fail,E0599
//! use carina_core::provider::{HostError, ProviderError};
//! use carina_core::provider_text::{HostDiagnostic, ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")] // error-chain: caller-renders-sources
//! struct Wrapper { #[source] source: ProviderDiagnostic }
//! ```
//!
//! ```
//! use carina_core::provider::{HostError, ProviderError};
//! use carina_core::provider_text::{HostDiagnostic, ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")] // error-chain: caller-renders-sources
//! struct Wrapper { #[source] source: HostDiagnostic }
//! ```
//!
//! Partial reasons require checked missing attributes, with the state fixed explicitly.
//!
//! ```compile_fail,E0308
//! use carina_core::provider::{NonEmptyMissingAttributes, PartialReadDiagnostic};
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn partial(reason: ProviderDiagnostic, missing: NonEmptyMissingAttributes) {
//!     let _: PartialReadDiagnostic = PartialReadDiagnostic::new(reason, Vec::<String>::new());
//! }
//! ```
//!
//! ```
//! use carina_core::provider::{NonEmptyMissingAttributes, PartialReadDiagnostic};
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn partial(reason: ProviderDiagnostic, missing: NonEmptyMissingAttributes) {
//!     let _: PartialReadDiagnostic = PartialReadDiagnostic::new(reason, missing);
//! }
//! ```
//!
//! Update partials require checked missing attributes, with the state fixed explicitly.
//!
//! ```compile_fail,E0308
//! use carina_core::provider::{AppliedState, NonEmptyMissingAttributes, UpdateOutcome};
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn partial(state: AppliedState, reason: ProviderDiagnostic, missing: NonEmptyMissingAttributes) {
//!     let _: UpdateOutcome = UpdateOutcome::partial_success(state, reason, vec![]);
//! }
//! ```
//!
//! ```
//! use carina_core::provider::{AppliedState, NonEmptyMissingAttributes, UpdateOutcome};
//! use carina_core::provider_text::ProviderDiagnostic;
//! fn partial(state: AppliedState, reason: ProviderDiagnostic, missing: NonEmptyMissingAttributes) {
//!     let _: UpdateOutcome = UpdateOutcome::partial_success(state, reason, missing);
//! }
//! ```
//!
//! Runtime context cannot mint bound text.
//!
//! ```compile_fail,E0277
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal_caused_by(ProviderText::new("host context"), HostError::internal("host cause"));
//! ```
//!
//! ```
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal_caused_by("host context", HostError::internal("host cause"));
//! ```
//!
//! Runtime opaque messages cannot mint bound text.
//!
//! ```compile_fail,E0277
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal(ProviderText::new("host message"));
//! ```
//!
//! ```
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal("host message");
//! ```
//!
//! Opaque context has no plaintext formatter.
//!
//! ```compile_fail,E0277
//! use carina_core::provider_text::ProviderContext;
//! let context: ProviderContext = "static".into();
//! let _ = format!("{context}");
//! ```
//!
//! ```
//! use carina_core::provider_text::ProviderContext;
//! let context: ProviderContext = "static".into();
//! let _ = format!("{context:?}");
//! ```
//!
//! Test predicates cannot capture an output channel.
//!
//! ```compile_fail,E0308
//! use carina_core::provider_text::ProviderDiagnostic;
//! use std::cell::Cell;
//! let result = Cell::new(false);
//! let cause = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! assert!(cause.as_cause().matches_for_test::<std::io::Error>(|_| { result.set(true); true }));
//! ```
//!
//! ```
//! use carina_core::provider_text::ProviderDiagnostic;
//! use std::cell::Cell;
//! let result = Cell::new(false);
//! let cause = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! assert!(cause.as_cause().matches_for_test::<std::io::Error>(|_| true));
//! ```
//!
//! Classified adapters can only be built by their constructor.
//!
//! ```compile_fail,E0451
//! use carina_core::provider_text::{ClassifiedSource, Retryability};
//! let _ = ClassifiedSource { cause: std::io::Error::other("leaf"), retryability: Retryability::Transient };
//! ```
//!
//! ```
//! use carina_core::provider_text::{ClassifiedSource, Retryability};
//! let _ = ClassifiedSource::new(std::io::Error::other("leaf"), Retryability::Transient);
//! ```
//!
//! Boolean retry setters have been replaced by explicit policy.
//!
//! ```compile_fail,E0599
//! use carina_core::{provider::ProviderError, provider_text::Retryability};
//! let _ = ProviderError::api_error("message").with_transient(true);
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::Retryability};
//! let _ = ProviderError::api_error("message").with_retryability(Retryability::Transient);
//! ```
//!
//! No-added-context adoption has exactly two arguments.
//!
//! ```compile_fail,E0061
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! let _ = ProviderError::from_diagnostic(ProviderErrorKind::Internal, std::io::Error::other("leaf"), "extra context");
//! ```
//!
//! ```
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! let _ = ProviderError::from_diagnostic(ProviderErrorKind::Internal, std::io::Error::other("leaf"));
//! ```
//!
//! Independent causal parents are intentionally unsupported.
//!
//! ```compile_fail,E0277
//! use carina_core::provider::ProviderError;
//! fn causal_parent() {
//!     let _ = ProviderError::api_error_caused_by("operation failed", (std::io::Error::other("one"), std::io::Error::other("two")));
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! fn causal_parent() {
//!     let _ = ProviderError::api_error_caused_by("operation failed", std::io::Error::other("one"));
//! }
//! ```
//!
//! TextState::message cannot construct host text outside core.
//!
//! ```compile_fail,E0576
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostErrorText = <Bound as TextState>::message(String::from("host text"));
//! ```
//!
//! ```
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostErrorText = HostErrorText::new(String::from("host text"));
//! ```
//!
//! TextState::context cannot construct host text outside core.
//!
//! ```compile_fail,E0576
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostErrorText = <Bound as TextState>::context("host context");
//! ```
//!
//! ```
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostErrorText = HostErrorText::new("host context");
//! ```
//!
//! TextState::partial_read_reason cannot construct host text outside core.
//!
//! ```compile_fail,E0576
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostDiagnostic = <Bound as TextState>::partial_read_reason(String::from("guest text"));
//! ```
//!
//! ```
//! use carina_core::provider::HostError;
//! use carina_core::provider_text::{Bound, HostDiagnostic, HostErrorText, TextState};
//! let _: HostDiagnostic = HostDiagnostic::new(HostError::internal("host-authored error"));
//! ```
//!
//! Generic TextState bounds cannot mint a diagnostic from raw reasons.
//!
//! ```compile_fail,E0599
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     S::partial_read_reason(reason)
//! }
//! ```
//!
//! ```
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     admitted
//! }
//! ```
//!
//! ClassifiedSource does not format an arbitrary foreign cause.
//!
//! ```compile_fail,E0277
//! use carina_core::provider_text::{ClassifiedSource, ProviderDiagnostic, Retryability};
//! let source = ClassifiedSource::new(std::io::Error::other("leaf"), Retryability::Transient);
//! let _ = format!("{source:?}");
//! ```
//!
//! ```
//! use carina_core::provider_text::{ClassifiedSource, ProviderDiagnostic, Retryability};
//! let source = ProviderDiagnostic::new(ClassifiedSource::new(std::io::Error::other("leaf"), Retryability::Transient));
//! let _ = format!("{source:?}");
//! ```
//!
//! Generic callers cannot invoke the sealed helper's construction methods.
//!
//! ```compile_fail,E0624
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     S::operations().partial_read_reason(reason)
//! }
//! ```
//!
//! ```
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     admitted
//! }
//! ```
//!
//! Generic callers cannot extract construction function pointers either.
//!
//! ```compile_fail,E0616
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     (S::operations().partial_read_reason)(reason)
//! }
//! ```
//!
//! ```
//! use carina_core::provider_text::{Diagnostic, TextState};
//! fn promote<S: TextState>(reason: S::PartialReadReason, admitted: Diagnostic<S>) -> Diagnostic<S> {
//!     admitted
//! }
//! ```
//!
