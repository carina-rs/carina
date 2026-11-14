//! Public GitHub addresses are constructible from validated components.
use carina_state::StateUrl;

#[test]
fn github_round1_url_variant_is_constructible_with_validated_fields() {
    let parsed = StateUrl::parse("github://owner/repo/state.json?branch=states%2Fprod").unwrap();
    let StateUrl::GitHub {
        repository,
        branch,
        path,
        ..
    } = parsed.clone()
    else {
        panic!("github")
    };
    let constructed = StateUrl::GitHub {
        repository,
        branch,
        path,
    };
    assert_eq!(constructed, parsed);
}

#[test]
fn github_round1_credential_errors_keep_distinct_types_and_sources() {
    use carina_core::github_auth::GitHubTokenError;
    use carina_state::GitHubError;
    use std::error::Error;
    for (error, deadline) in [
        (GitHubTokenError::Deadline, true),
        (GitHubTokenError::Unavailable, false),
    ] {
        let mapped = GitHubError::from(error);
        assert_eq!(
            matches!(mapped, GitHubError::CredentialsDeadline { .. }),
            deadline
        );
        assert_eq!(
            matches!(mapped, GitHubError::CredentialsUnavailable { .. }),
            !deadline
        );
        assert!(
            mapped
                .source()
                .and_then(|source| source.downcast_ref::<GitHubTokenError>())
                .is_some()
        );
        let message = mapped.to_string();
        assert!(
            message.contains("gh auth token --hostname github.com"),
            "{message}"
        );
        if deadline {
            assert!(message.contains("deadline"), "{message}");
        } else {
            assert!(
                message.contains("GITHUB_TOKEN") && message.contains("gh auth login"),
                "{message}"
            );
        }
    }
}

#[test]
fn github_round4_invalid_token_error_keeps_its_typed_source() {
    use carina_core::github_auth::GitHubTokenError;
    use carina_state::GitHubError;
    use std::error::Error;
    let error = GitHubError::from(GitHubTokenError::InvalidCharacters);
    assert!(matches!(error, GitHubError::CredentialsUnavailable { .. }));
    assert!(matches!(
        error.source().unwrap().downcast_ref::<GitHubTokenError>(),
        Some(GitHubTokenError::InvalidCharacters)
    ));
    assert!(
        error
            .to_string()
            .contains("token contains invalid characters")
    );
}
