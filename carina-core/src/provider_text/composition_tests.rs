use super::*;
use crate::provider::{ProviderError, ProviderErrorKind};

fn protected(text: &str, secrets: &RedactionSet) -> ProviderText {
    ErrorText::<Unbound>::from_provider_text(ProviderText::new(text))
        .into_host(secrets)
        .opaque()
}

#[test]
fn context_cause_and_metadata_union_is_live_at_every_opaque_frame() {
    let context_set = RedactionSet::for_test(&["context-private"]);
    let cause_set = RedactionSet::for_test(&["cause-private"]);
    let metadata_set = RedactionSet::for_test(&["metadata-private"]);
    let boundary = RedactionSet::for_test(&["boundary-private"]);
    let raw = "context-private cause-private metadata-private boundary-private later-private";
    let context = protected(raw, &context_set).with_context(protected(raw, &metadata_set));
    let cause: ProviderError =
        ProviderError::api_error_caused_by("read", std::io::Error::other(raw))
            .with_secrets(&cause_set)
            .into();
    let error = ProviderError::api_error_caused_by(context, ProviderDiagnostic::new(cause));
    let error =
        ProviderError::from_diagnostic(ProviderErrorKind::Internal, ProviderDiagnostic::new(error))
            .with_operation(raw)
            .with_request_id(raw);
    let host = error.with_secrets(&boundary);
    let assert_redacted = |text: &str| {
        for secret in [
            "context-private",
            "cause-private",
            "metadata-private",
            "boundary-private",
        ] {
            assert!(!text.contains(secret), "{text}");
        }
    };
    assert_redacted(&host.to_string());
    assert_redacted(&format!("{host:?} {:?}", host.to_wire()));
    let mut node = host.detail().cause().unwrap().as_cause();
    loop {
        assert_redacted(node.redact(&boundary).as_str());
        if node.is::<std::io::Error>() {
            break;
        }
        node = node
            .source()
            .expect("traverse the captured/provider/bound frames");
    }
    assert!(host.to_string().contains("later-private"));
    context_set.extend(&RedactionSet::for_test(&["later-private"]));
    assert!(
        !host.to_string().contains("later-private"),
        "cached projections must observe inventory generations"
    );
    assert!(!node.redact(&boundary).as_str().contains("later-private"));
    assert!(
        !cause_set.snapshot().iter().any(|s| s == "context-private"),
        "union must never extend a source inventory"
    );
}

#[test]
fn diagnostic_clones_share_causal_storage_and_own_their_text_views() {
    let carried = RedactionSet::for_test(&["carried-private"]);
    let failure: ProviderError = ProviderError::api_error_caused_by(
        "read",
        std::io::Error::other("first-private second-private carried-private"),
    )
    .with_secrets(&carried)
    .into();
    let original = ProviderDiagnostic::new(failure);
    let clone = original.clone();
    let original_context = original.text.redaction_context();
    let clone_context = clone.text.redaction_context();
    assert_eq!(original_context.0.len(), 1);
    assert_eq!(clone_context.0.len(), original_context.0.len());
    assert!(
        original_context
            .0
            .iter()
            .zip(&clone_context.0)
            .all(|(a, b)| a.shares_inventory(b))
    );
    assert!(original_context.0[0].shares_inventory(&carried));
    assert!(std::sync::Arc::ptr_eq(&original.error, &clone.error));
    assert!(!std::ptr::eq(original.text.as_ref(), clone.text.as_ref()));
    let first = original.with_secrets(&RedactionSet::for_test(&["first-private"]));
    let second = clone.with_secrets(&RedactionSet::for_test(&["second-private"]));
    assert_eq!(first.to_string(), "read: (secret) second-private (secret)");
    assert_eq!(second.to_string(), "read: first-private (secret) (secret)");
}

#[test]
fn capturing_an_existing_bound_diagnostic_preserves_the_current_node() {
    let original = ProviderDiagnostic::new(std::io::Error::other("leaf"))
        .with_secrets(&RedactionSet::for_test(&[]));
    let adopted = HostDiagnostic::new(original.clone());
    assert!(std::sync::Arc::ptr_eq(&original.error, &adopted.error));
    assert!(adopted.is::<std::io::Error>());
}

#[derive(Debug, thiserror::Error)]
#[error("foreign validation wrapper")] // error-chain: caller-renders-sources
struct ForeignValidation(#[source] crate::schema::ProviderValidation);

#[derive(Debug, thiserror::Error)]
#[error("foreign diagnostic wrapper")] // error-chain: caller-renders-sources
struct ForeignDiagnostic(#[source] HostDiagnostic);

#[test]
fn foreign_wrapper_inherits_the_nearest_classified_diagnostic() {
    let retryability = Retryability::Transient;
    let leaf = ProviderError::api_error("leaf").with_retryability(Retryability::Transient);
    let nearest =
        ProviderError::internal_caused_by("validation", leaf).with_retryability(retryability);
    let wrapped = ForeignValidation(ProviderDiagnostic::new(nearest).into());
    let error = ProviderError::invalid_input_caused_by("outer", wrapped);
    assert_eq!(error.retryability(), retryability);
    let host = error.with_secrets(&RedactionSet::for_test(&[]));
    assert_eq!(host.retryability(), retryability);
    assert!(
        host.detail()
            .cause()
            .unwrap()
            .source_is::<crate::schema::ProviderValidation>()
    );
    let diagnostic =
        ProviderDiagnostic::new(ProviderError::api_error("leaf").with_retryability(retryability))
            .with_secrets(&RedactionSet::for_test(&[]));
    let error = ProviderError::api_error_caused_by("outer", ForeignDiagnostic(diagnostic));
    assert_eq!(error.retryability(), retryability);
}

#[test]
fn unclassified_foreign_frame_does_not_hide_a_transient_descendant() {
    let leaf = ProviderError::api_error("leaf").with_retryability(Retryability::Transient);
    let nearest = ProviderError::internal_caused_by("validation", leaf)
        .with_retryability(Retryability::Unclassified);
    assert!(
        !nearest.is_transient(),
        "an explicit override applies to this error"
    );
    let wrapped = ForeignValidation(ProviderDiagnostic::new(nearest).into());
    let error = ProviderError::invalid_input_caused_by("outer", wrapped);
    assert!(error.is_transient());
    assert!(
        error
            .with_secrets(&RedactionSet::for_test(&[]))
            .is_transient()
    );
}

#[test]
fn changing_redaction_generations_replaces_the_cached_projection() {
    let secrets = RedactionSet::for_test(&[]);
    let text = ErrorText::<Unbound>::new("last-secret").into_host(&secrets);
    let first = text.as_diagnostic();
    let stale = std::sync::Arc::downgrade(&text.projection());
    for generation in 0..128 {
        let secret = if generation == 127 {
            "last-secret".to_owned()
        } else {
            format!("secret-{generation}")
        };
        secrets.extend(&RedactionSet::for_test(&[&secret]));
        let rendered = text.to_string();
        assert!(
            stale.upgrade().is_none(),
            "replacing the cache must release the previous projection"
        );
        if generation == 127 {
            assert_eq!(rendered, "(secret)");
        }
        assert_eq!(text.cached_projection_count(), 1);
    }
    assert_eq!(
        first.as_str(),
        "last-secret",
        "owned snapshots remain valid after cache replacement"
    );
}
