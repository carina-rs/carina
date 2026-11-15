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
fn github_credential_errors_keep_distinct_types_and_terminal_details() {
    use carina_core::github_auth::GitHubTokenError;
    use carina_state::GitHubError;
    use std::error::Error;
    for (error, deadline) in [
        (GitHubTokenError::Deadline, true),
        (GitHubTokenError::Unavailable, false),
    ] {
        let detail = error.to_string();
        let mapped = GitHubError::from(error);
        assert_eq!(
            matches!(mapped, GitHubError::CredentialsDeadline { .. }),
            deadline
        );
        assert_eq!(
            matches!(mapped, GitHubError::CredentialsUnavailable { .. }),
            !deadline
        );
        assert!(mapped.source().is_none());
        let message = mapped.to_string();
        assert_eq!(message.matches(&detail).count(), 1, "{message}");
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
fn github_invalid_token_error_renders_its_terminal_detail_once() {
    use carina_core::github_auth::GitHubTokenError;
    use carina_state::GitHubError;
    use std::error::Error;
    let detail = GitHubTokenError::InvalidCharacters.to_string();
    let error = GitHubError::from(GitHubTokenError::InvalidCharacters);
    assert!(matches!(error, GitHubError::CredentialsUnavailable { .. }));
    assert!(error.source().is_none());
    assert_eq!(error.to_string().matches(&detail).count(), 1, "{error}");
    assert!(
        error
            .to_string()
            .contains("token contains invalid characters")
    );
}
