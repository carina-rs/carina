//! Composition API admission rules, with a compiling twin for every rejection.
//!
//! invalid_input requires opaque owned runtime context.
//!
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
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
//! ```compile_fail
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _: ProviderText = ProviderError::internal("leaf").into();
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal_caused_by(ProviderText::new("context"), ProviderError::internal("leaf"));
//! ```
//!
//! Diagnostics cannot be projected into text.
//!
//! ```compile_fail
//! use carina_core::provider_text::{ProviderText, ProviderDiagnostic};
//! let _: ProviderText = ProviderDiagnostic::new(std::io::Error::other("leaf")).into();
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! let _ = ProviderError::internal_caused_by("context", ProviderDiagnostic::new(std::io::Error::other("leaf")));
//! ```
//!
//! Errors cannot become context.
//!
//! ```compile_fail
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! use carina_core::provider_text::ProviderContext;
//! let _: ProviderContext = ProviderError::internal("leaf").into();
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! use carina_core::provider_text::ProviderContext;
//! let _: ProviderContext = ProviderText::new("runtime").into();
//! ```
//!
//! Diagnostics cannot become context.
//!
//! ```compile_fail
//! use carina_core::provider_text::{ProviderContext, ProviderDiagnostic};
//! let _: ProviderContext = ProviderDiagnostic::new(std::io::Error::other("leaf")).into();
//! ```
//!
//! ```
//! use carina_core::provider_text::ProviderContext;
//! let _: ProviderContext = "static context".into();
//! ```
//!
//! Captured diagnostic text cannot be replaced.
//!
//! ```compile_fail
//! use carina_core::provider_text::{ProviderDiagnostic, ProviderText};
//! let _ = ProviderDiagnostic::from_text(std::io::Error::other("leaf"), ProviderText::new("replacement"));
//! ```
//!
//! ```
//! use carina_core::provider_text::ProviderDiagnostic;
//! let _ = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! ```
//!
//! Message-only kind constructors accept opaque text directly.
//!
//! ```compile_fail
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! use carina_core::provider::ProviderErrorKind;
//! let _ = ProviderError::from_text(ProviderErrorKind::Internal, ProviderText::new("message"));
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal(ProviderText::new("message"));
//! ```
//!
//! Failure bodies stay private.
//!
//! ```compile_fail
//! use carina_core::provider::FailureBody;
//! let _ = FailureBody::Message("replacement".into());
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! use carina_core::provider_text::ProviderText;
//! let _ = ProviderError::internal("message");
//! ```
//!
//! The opaque adapter stays private.
//!
//! ```compile_fail
//! use carina_core::{provider::ProviderError, provider_text::OpaqueProviderFailure};
//! let _ = OpaqueProviderFailure(ProviderError::internal("leaf"));
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! let _ = ProviderDiagnostic::new(ProviderError::internal("leaf"));
//! ```
//!
//! Unbound ProviderError is data, not a local Error source.
//!
//! ```compile_fail
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")]
//! struct Wrapper { #[source] source: ProviderError }
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! #[derive(Debug)]
//! struct Failure { source: ProviderError }
//! ```
//!
//! Unbound ProviderDiagnostic is data, not a local Error source.
//!
//! ```compile_fail
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! #[derive(Debug, thiserror::Error)]
//! #[error("wrapper")]
//! struct Wrapper { #[source] source: ProviderDiagnostic }
//! ```
//!
//! ```
//! use carina_core::{provider::ProviderError, provider_text::ProviderDiagnostic};
//! #[derive(Debug)]
//! struct Failure { source: ProviderDiagnostic }
//! ```
//!
//! Validate missing attributes before consuming the partial reason.
//!
//! ```compile_fail
//! use carina_core::{provider::PartialReadDiagnostic, provider_text::ProviderDiagnostic};
//! let _ = PartialReadDiagnostic::new(ProviderDiagnostic::message("reason".into()), Vec::<String>::new());
//! ```
//!
//! ```
//! use carina_core::{provider::{NonEmptyMissingAttributes, PartialReadDiagnostic}, provider_text::ProviderDiagnostic};
//! let missing = NonEmptyMissingAttributes::try_from(vec!["name".to_owned()]).unwrap();
//! let _: PartialReadDiagnostic = PartialReadDiagnostic::new(ProviderDiagnostic::message("reason".into()), missing);
//! ```
//!
//! Update partials also require checked attributes.
//!
//! ```compile_fail
//! use carina_core::{provider::{AppliedState, UpdateOutcome}, provider_text::ProviderDiagnostic};
//! fn partial(state: AppliedState) {
//!     let _ = UpdateOutcome::partial_success(state, ProviderDiagnostic::message("reason".into()), vec![]);
//! }
//! ```
//!
//! ```
//! use carina_core::{provider::{AppliedState, NonEmptyMissingAttributes, UpdateOutcome}, provider_text::ProviderDiagnostic};
//! fn partial(state: AppliedState) {
//!     let missing = NonEmptyMissingAttributes::try_from(vec!["name".to_owned()]).unwrap();
//!     let _: UpdateOutcome = UpdateOutcome::partial_success(state, ProviderDiagnostic::message("reason".into()), missing);
//! }
//! ```
//!
//! Runtime context cannot mint bound text.
//!
//! ```compile_fail
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal_caused_by(ProviderText::new("guest text"), HostError::internal("host cause"));
//! ```
//!
//! ```
//! use carina_core::provider::HostError;
//! let _ = HostError::internal_caused_by("host context", HostError::internal("host cause"));
//! ```
//!
//! Runtime opaque messages cannot mint bound text.
//!
//! ```compile_fail
//! use carina_core::{provider::HostError, provider_text::ProviderText};
//! let _ = HostError::internal(ProviderText::new("guest text"));
//! ```
//!
//! ```
//! use carina_core::provider::HostError;
//! let _ = HostError::internal("host message");
//! ```
//!
//! Opaque context has no formatter.
//!
//! ```compile_fail
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
//! ```compile_fail
//! use carina_core::provider_text::ProviderDiagnostic;
//! use std::cell::Cell;
//! let result = Cell::new(false);
//! let cause = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! cause.as_cause().matches_for_test::<std::io::Error>(|_| { result.set(true); true });
//! ```
//!
//! ```
//! use carina_core::provider_text::ProviderDiagnostic;
//! let cause = ProviderDiagnostic::new(std::io::Error::other("leaf"));
//! assert!(cause.as_cause().matches_for_test::<std::io::Error>(|_| true));
//! ```
//!

//! Classified adapters can only be built by their constructor.
//!
//! ```compile_fail
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
//! ```compile_fail
//! use carina_core::provider::ProviderError;
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
//! ```compile_fail
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! let _ = ProviderError::from_diagnostic(ProviderErrorKind::Internal, "context", std::io::Error::other("leaf"));
//! ```
//!
//! ```
//! use carina_core::provider::{ProviderError, ProviderErrorKind};
//! let _ = ProviderError::from_diagnostic(ProviderErrorKind::Internal, std::io::Error::other("leaf"));
//! ```

//! Independent causal parents are intentionally unsupported.
//!
//! ```compile_fail
//! use carina_core::provider::ProviderError;
//! fn independent_causes_are_not_a_diagnostic_source() {
//!     let _ = ProviderError::api_error_caused_by(
//!         "operation failed", (std::io::Error::other("one"), std::io::Error::other("two")),
//!     );
//! }
//! ```
//!
//! ```
//! use carina_core::provider::ProviderError;
//! fn one_causal_parent_is_supported() {
//!     let _ = ProviderError::api_error_caused_by("operation failed", std::io::Error::other("one"));
//! }
//! ```
