use super::*;
use crate::provider::{HostError, ProviderError};

#[test]
fn review_round7_rebinding_retains_each_original_inventory() {
    let first = RedactionSet::for_test(&["first-private"]);
    let second = RedactionSet::for_test(&["second-private"]);
    let message =
        ProviderError::api_error("first-private second-private third-private").with_secrets(&first);
    let relayed: ProviderError = message.into();
    let message = relayed.with_secrets(&second);
    let host = HostError::internal_caused_by("host context", message);
    let relayed: ProviderError = host.into();
    let rebound = relayed.with_secrets(&RedactionSet::for_test(&["third-private"]));
    let text = rebound.to_string();
    assert!(
        !text.contains("first-private"),
        "lost first binding: {text}"
    );
    assert!(
        !text.contains("second-private"),
        "lost second binding: {text}"
    );
    assert!(!text.contains("third-private"), "lost new binding: {text}");
    assert!(text.contains("host context"));
    assert!(text.contains("(secret)"));
}

#[test]
fn review_round7_opaque_and_diagnostic_relays_retain_binding() {
    let secrets = RedactionSet::for_test(&["relay-private"]);
    let empty = RedactionSet::before_request();
    let bound = ProviderError::api_error("relay-private").with_secrets(&secrets);
    let diagnostic = ProviderDiagnostic::new(bound.clone());
    let host = ProviderError::internal_caused_by("public context", bound).with_secrets(&empty);
    assert_eq!(host.to_string(), "public context: (secret)");
    let opaque = diagnostic.opaque();
    let rebound = ErrorText::from_provider_text(opaque).into_host(&empty);
    assert_eq!(rebound.to_string(), "(secret)");
}

#[test]
fn review_round7_opaque_keeps_multiple_segments_and_live_inventories() {
    let first = RedactionSet::for_test(&["first-private"]);
    let second = RedactionSet::for_test(&["second-private"]);
    let mut text = ErrorText::new("first-private later-private ").into_host(&first);
    text.append(&ErrorText::new("second-private").into_host(&second));
    let relayed = text.host_for_state::<Unbound>();
    first.extend(&RedactionSet::for_test(&["later-private"]));
    let rebound = relayed.into_host(&RedactionSet::before_request());
    assert_eq!(rebound.to_string(), "(secret) (secret) (secret)");
}

#[test]
fn review_round7_duplicate_extend_keeps_generation_and_snapshot() {
    let secrets = RedactionSet::for_test(&["private", "other-private"]);
    let snapshot = secrets.snapshot();
    let generation = secrets.generation();
    secrets.extend(&RedactionSet::for_test(&["private"]));
    assert_eq!(secrets.generation(), generation);
    assert!(std::sync::Arc::ptr_eq(&snapshot, &secrets.snapshot()));
}

#[test]
fn review_round7_stream_refreshes_only_changed_inventory_and_rechecks_tail() {
    let secrets = RedactionSet::for_test(&["longest-existing-spelling-for-lookahead"]);
    let mut stream = RedactionStream::new(secrets.clone());
    let mut output = String::new();
    stream.push_str("new-", &mut |chunk| output.push_str(chunk));
    assert!(output.is_empty());
    secrets.extend(&RedactionSet::for_test(&["new-private"]));
    stream.push_str("private\n", &mut |chunk| output.push_str(chunk));
    stream.push_str("public\n", &mut |chunk| output.push_str(chunk));
    stream.flush(&mut |chunk| output.push_str(chunk));
    assert_eq!(output, "(secret)\npublic\n");
    assert_eq!(stream.snapshot_locks, 2);
    assert_eq!(stream.refresh_checks, 4);
}

#[test]
fn review_round7_required_permissions_enforce_exact_ascii_grammar() {
    let secrets = RedactionSet::for_test(&["PermissionPrivate4305"]);
    for action in [
        "s3:GetObject",
        "ec2:Describe*",
        "service-123:A0*Z",
        "a:*",
        "S3:Get",
    ] {
        let permission = RequiredPermission::admit(action.into(), 0, &secrets).unwrap();
        assert_eq!(permission.iam_action(), action);
        assert_eq!(permission.to_string(), action);
    }
    for action in [
        "",
        "s3",
        ":Get",
        "s3:",
        "s_3:Get",
        "s3:Get:Object",
        "s3:Get Object",
        "s3:Get\nObject",
        "s3:Get\rObject",
        "s3:Get\tObject",
        "s3:Get?",
        "s3:Get_",
        "s3:Get-",
        "s3:Get/",
        "s3:Geté",
        "é:Get",
        "s3:Get\0",
    ] {
        let error = RequiredPermission::admit(action.into(), 17, &secrets).unwrap_err();
        assert_eq!(error.index(), 17);
        assert_eq!(error.to_string(), "invalid required permission at index 17");
    }
    let permission =
        RequiredPermission::admit("iam:PermissionPrivate4305".into(), 0, &secrets).unwrap();
    assert_eq!(permission.to_string(), "iam:(secret)");
    assert!(!format!("{permission:?}").contains("PermissionPrivate4305"));
    assert_eq!(permission.iam_action(), "iam:PermissionPrivate4305");
}

#[test]
fn review_round7_rebound_structured_resource_wire_retains_secrets() {
    let secrets = RedactionSet::for_test(&["wire-private"]);
    let original = ProviderError::api_error("public message")
        .for_resource(crate::resource::ResourceId::with_provider_identity(
            "wire-private",
            "test.Resource",
            "wire-private",
            None,
        ))
        .with_secrets(&secrets);
    let relayed: ProviderError = original.into();
    let rebound = relayed.with_secrets(&RedactionSet::before_request());
    let wire = serde_json::to_string(&rebound.to_wire()).unwrap();
    assert!(
        !wire.contains("wire-private"),
        "structured wire leaked: {wire}"
    );
    assert!(wire.contains("(secret)"));
}

#[test]
fn review_round7_rebound_typed_normalization_retains_secrets() {
    use carina_provider_protocol::normalization::{NormalizationError, NormalizationErrorKind};
    let original = ProviderError::internal_caused_by(
        "public message",
        NormalizationError::new(
            carina_provider_protocol::types::ResourceId {
                provider: "public".into(),
                resource_type: "Resource".into(),
                identity: "projection-private".into(),
            },
            "projection-private",
            NormalizationErrorKind::DuplicateKey {
                key: "projection-private".into(),
            },
        ),
    )
    .with_secrets(&RedactionSet::for_test(&["projection-private"]));
    let relayed: ProviderError = original.into();
    let rebound = relayed.with_secrets(&RedactionSet::before_request());
    let cause = rebound.detail().cause().unwrap().normalization().unwrap();
    let projected = format!("{cause:?}");
    assert!(
        !projected.contains("projection-private"),
        "typed projection leaked: {projected}"
    );
    assert!(projected.contains("(secret)"));
}

#[test]
fn review_round7_cause_view_retains_live_carried_inventory() {
    let secrets = RedactionSet::before_request();
    let original =
        ProviderError::internal_caused_by("public message", std::io::Error::other("later-private"))
            .with_secrets(&secrets);
    let relayed: ProviderError = original.into();
    let rebound = relayed.with_secrets(&RedactionSet::before_request());
    let view = rebound.detail().cause().unwrap().as_cause();
    secrets.extend(&RedactionSet::for_test(&["later-private"]));
    let projected = view
        .project_redacted::<std::io::Error>(|cause| cause.to_string())
        .unwrap();
    assert_eq!(projected.as_str(), "(secret)");
}
