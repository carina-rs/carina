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

#[cfg(feature = "guest-runtime")]
#[derive(Debug)]
struct CompleteGuestDisplay {
    source: RenderedGuestDiagnostic,
}

#[cfg(feature = "guest-runtime")]
impl std::fmt::Display for CompleteGuestDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("guest-secret public reason")
    }
}

#[cfg(feature = "guest-runtime")]
impl std::error::Error for CompleteGuestDisplay {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[cfg(feature = "guest-runtime")]
#[test]
fn safe_rendered_guest_frame_preserves_redactions_and_typed_cause() {
    let carried = RedactionSet::for_test(&["guest-secret"]);
    let source: ProviderError = ProviderError::api_error_caused_by(
        ProviderText::new("metadata guest-secret").with_redaction(&carried),
        std::io::Error::other("guest-secret public reason"),
    )
    .with_request_id("guest-secret")
    .with_retryability(Retryability::Transient)
    .with_secrets(&carried)
    .into();
    let frame = RenderedGuestDiagnostic::from(ProviderDiagnostic::new(source));
    assert!(!format!("{frame} {frame:?}").contains("guest-secret"));
    let error =
        ProviderError::internal_caused_by("load failed", CompleteGuestDisplay { source: frame });
    assert!(error.is_transient());
    assert!(
        error
            .detail()
            .cause()
            .unwrap()
            .source_is::<std::io::Error>()
    );
    let boundary = RedactionSet::for_test(&[]);
    let host = error.with_secrets(&boundary);
    let wire = host.to_wire();
    let text = format!("{host} {host:?} {wire:?}");
    assert!(!text.contains("guest-secret"), "{text}");
    assert!(text.contains("(secret)"), "{text}");
    assert_eq!(
        wire.cause
            .as_ref()
            .unwrap()
            .matches("public reason")
            .count(),
        1
    );
    let mut node = host.detail().cause().unwrap().as_cause();
    loop {
        assert!(!node.redact(&boundary).as_str().contains("guest-secret"));
        if node.is::<std::io::Error>() {
            break;
        }
        node = node
            .source()
            .expect("the typed guest cause remains traversable");
    }
}

#[cfg(feature = "guest-runtime")]
#[test]
fn safe_rendered_guest_frame_renders_the_captured_text_once() {
    let source = ProviderDiagnostic::new(std::io::Error::other("guest-secret public reason"));
    let frame = RenderedGuestDiagnostic::from(source.clone());
    assert!(std::sync::Arc::ptr_eq(&frame.0.error, &source.error));
    let error = ProviderError::from_diagnostic(
        ProviderErrorKind::Internal,
        CompleteGuestDisplay { source: frame },
    );
    let host = error.with_secrets(&RedactionSet::for_test(&["guest-secret"]));
    assert_eq!(host.to_string(), "(secret) public reason");
    assert_eq!(host.to_wire().message, "(secret) public reason");
    assert!(host.detail().cause().unwrap().source_is::<std::io::Error>());
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
fn explicit_unclassified_below_a_foreign_frame_is_final() {
    let leaf = ProviderError::api_error("leaf").with_retryability(Retryability::Transient);
    let nearest = ProviderError::internal_caused_by("validation", leaf)
        .with_retryability(Retryability::Unclassified);
    assert!(
        !nearest.is_transient(),
        "an explicit override applies to this error"
    );
    let wrapped = ForeignValidation(ProviderDiagnostic::new(nearest).into());
    let error = ProviderError::invalid_input_caused_by("outer", wrapped);
    assert_eq!(error.retryability(), Retryability::Unclassified);
    assert_eq!(
        error
            .with_secrets(&RedactionSet::for_test(&[]))
            .retryability(),
        Retryability::Unclassified,
    );
}

#[test]
fn structural_validation_wrappers_inherit_their_childs_retryability() {
    use crate::schema::TypeError;

    for retryability in [
        Retryability::Transient,
        Retryability::DependencyBlocked,
        Retryability::Unclassified,
    ] {
        let leaf = TypeError::ValidationFailed {
            message: ProviderDiagnostic::new(ForeignDiagnostic(
                ProviderDiagnostic::new(
                    ProviderError::internal_caused_by(
                        "nearest",
                        ProviderError::api_error("leaf").with_retryability(Retryability::Transient),
                    )
                    .with_retryability(retryability),
                )
                .with_secrets(&RedactionSet::for_test(&[])),
            ))
            .into(),
        };
        for category in [
            TypeError::ListItemError {
                index: 0,
                inner: Box::new(leaf.clone()),
            },
            TypeError::MapKeyError {
                key: "key".into(),
                inner: Box::new(leaf.clone()),
            },
            TypeError::MapValueError {
                key: "key".into(),
                inner: Box::new(leaf.clone()),
            },
            TypeError::StructFieldError {
                field: "field".into(),
                inner: Box::new(leaf.clone()),
            },
            TypeError::CustomTypeValidation {
                error: Box::new(
                    crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
                        owner: "test".into(),
                        identity: crate::schema::TypeIdentity::bare("Custom"),
                        error: Box::new(leaf),
                    },
                ),
                attribute: None,
            },
        ] {
            let error = ProviderError::invalid_input_caused_by("outer", category);
            assert_eq!(error.retryability(), retryability);
            let host = error.with_secrets(&RedactionSet::for_test(&[]));
            assert_eq!(host.retryability(), retryability);
        }
    }
}

#[test]
fn custom_type_validation_inherits_visible_and_hidden_child_retryability() {
    for masked in [false, true] {
        let category = crate::schema::TypeError::CustomTypeValidation {
            error: Box::new(crate::parser::CustomTypeValidationError::ProviderRejected {
                owner: "test".into(),
                identity: crate::schema::TypeIdentity::bare("Custom"),
                error: std::sync::Arc::new(
                    ProviderError::api_error("leaf")
                        .with_retryability(Retryability::Transient)
                        .with_secrets(&RedactionSet::for_test(&[])),
                ),
                masked,
            }),
            attribute: None,
        };
        let error = ProviderError::invalid_input_caused_by("outer", category);
        assert_eq!(error.retryability(), Retryability::Transient);
        let host = error.with_secrets(&RedactionSet::for_test(&[]));
        assert_eq!(host.retryability(), Retryability::Transient);
    }
}

fn assert_validation_alternatives_retryability(classes: [Retryability; 2], expected: Retryability) {
    use crate::schema::{TypeError, UnionArmFailures};

    let arm = |message, retryability| TypeError::ValidationFailed {
        message: ProviderDiagnostic::new(
            ProviderError::api_error(message).with_retryability(retryability),
        )
        .into(),
    };
    let failures =
        UnionArmFailures::new(arm("first", classes[0]), [arm("second", classes[1])]).unwrap();
    let direct = ProviderError::invalid_input_caused_by("outer", failures.clone());
    let wrapped = ProviderError::invalid_input_caused_by(
        "outer",
        TypeError::StructFieldError {
            field: "field".into(),
            inner: Box::new(TypeError::UnionAlternativesMismatch { failures }),
        },
    );
    for error in [direct, wrapped] {
        assert_eq!(error.retryability(), expected);
        let host = error.with_secrets(&RedactionSet::for_test(&[]));
        assert_eq!(host.retryability(), expected);
    }
}

#[test]
fn validation_alternatives_inherit_transient_when_every_arm_agrees() {
    assert_validation_alternatives_retryability(
        [Retryability::Transient, Retryability::Transient],
        Retryability::Transient,
    );
}

#[test]
fn validation_alternatives_with_transient_and_dependency_blocked_arms_are_unclassified() {
    assert_validation_alternatives_retryability(
        [Retryability::Transient, Retryability::DependencyBlocked],
        Retryability::Unclassified,
    );
}

#[test]
fn validation_alternatives_with_transient_and_unclassified_arms_are_unclassified() {
    assert_validation_alternatives_retryability(
        [Retryability::Transient, Retryability::Unclassified],
        Retryability::Unclassified,
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
