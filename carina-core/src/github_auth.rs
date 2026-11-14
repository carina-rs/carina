//! Shared GitHub.com credential precedence without provider or HTTP dependencies.
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitHubTokenError {
    #[error("Set GITHUB_TOKEN or run `gh auth login --hostname github.com`")]
    Unavailable,
    #[error("`gh auth token --hostname github.com` exceeded its deadline")]
    Deadline,
}

pub const TOKEN_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

pub fn get_github_token() -> Result<String, GitHubTokenError> {
    select_token(std::env::var("GITHUB_TOKEN").ok(), || {
        command_token(
            std::ffi::OsStr::new("gh"),
            &["auth", "token", "--hostname", "github.com"],
            TOKEN_LOOKUP_TIMEOUT,
        )
    })
}

fn select_token(
    environment: Option<String>,
    fallback: impl FnOnce() -> Result<Vec<u8>, GitHubTokenError>,
) -> Result<String, GitHubTokenError> {
    if let Some(token) = environment.filter(|token| !token.is_empty()) {
        return Ok(token);
    }
    let bytes = fallback()?;
    let token = String::from_utf8_lossy(&bytes).trim().to_owned();
    if token.is_empty() {
        return Err(GitHubTokenError::Unavailable);
    }
    Ok(token)
}

fn command_token(
    program: &std::ffi::OsStr,
    args: &[&str],
    budget: Duration,
) -> Result<Vec<u8>, GitHubTokenError> {
    use std::{
        io::Read,
        process::{Command, Stdio},
        sync::mpsc,
        time::Instant,
    };
    const MAX_TOKEN_BYTES: usize = 16_384;
    let deadline = Instant::now() + budget;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| GitHubTokenError::Unavailable)?;
    let stdout = child.stdout.take().expect("piped token output");
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_TOKEN_BYTES as u64 + 1)
            .read_to_end(&mut bytes);
        let _ = sender.send(result.map(|_| bytes));
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if result.is_err() {
                    GitHubTokenError::Unavailable
                } else {
                    GitHubTokenError::Deadline
                });
            }
        }
    };
    if !status.success() {
        return Err(GitHubTokenError::Unavailable);
    }
    let bytes = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| GitHubTokenError::Deadline)?
        .map_err(|_| GitHubTokenError::Unavailable)?;
    if bytes.len() > MAX_TOKEN_BYTES {
        return Err(GitHubTokenError::Unavailable);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_credentials_nonempty_environment_has_exact_precedence() {
        for token in ["explicit-token", " "] {
            assert_eq!(
                select_token(Some(token.into()), || panic!("must not invoke gh")).unwrap(),
                token
            );
        }
    }

    #[test]
    fn github_credentials_empty_or_missing_environment_use_trimmed_gh_output() {
        for environment in [None, Some(String::new())] {
            assert_eq!(
                select_token(environment, || Ok(b" token-from-gh\n".to_vec())).unwrap(),
                "token-from-gh"
            );
        }
        assert!(select_token(None, || Ok(b" \n".to_vec())).is_err());
        assert!(select_token(None, || Err(GitHubTokenError::Deadline)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn github_credentials_cli_lookup_is_bounded_and_never_reports_stderr() {
        let start = std::time::Instant::now();
        assert!(matches!(
            command_token(
                std::ffi::OsStr::new("/bin/sleep"),
                &["5"],
                Duration::from_millis(30)
            ),
            Err(GitHubTokenError::Deadline)
        ));
        assert!(start.elapsed() < Duration::from_secs(1));
        let error = command_token(
            std::ffi::OsStr::new("/bin/sh"),
            &["-c", "printf credential-do-not-report >&2; exit 1"],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("credential-do-not-report"));
    }
}
