//! Direct URL-addressed state loading.
//!
//! `carina state lookup`, `state list`, and `state show` accept a
//! `--state-url <URL>` flag that bypasses the `.crn` + `backend { ... }`
//! resolution path entirely (carina#3336). This module owns the URL
//! parsing and the read-only state loader that backs that flag.
//!
//! Three URL forms are accepted:
//!
//! | Form | Example | Resolution |
//! | ---- | ------- | ---------- |
//! | `s3://bucket/key` | `s3://my-states/prod/state.json` | Constructs an `S3Backend` via the shared region + SDK-client helpers in `s3.rs`. |
//! | `file://path` | `file:///tmp/state.json` | Reads the absolute path after the scheme. |
//! | Bare path | `./state.json`, `/abs/state.json` | Read directly from the local filesystem. |
//!
//! The parser surface is a [`StateUrl`] tagged enum, not a free
//! `&str`-taking function — every loader call must therefore go through
//! `StateUrl::parse`, which is the seam where the "bare path vs
//! scheme-prefixed" classification lives. The loader cannot be reached
//! with an unclassified raw string.

use std::path::PathBuf;

use crate::backend::{BackendError, BackendResult, StateBackend};
use crate::state::StateFile;

/// A parsed `--state-url` argument.
///
/// Constructable **only** via [`Self::parse`]: each variant is
/// `#[non_exhaustive]`, so external code cannot fabricate a `StateUrl`
/// with an unvalidated bucket / key / path. Combined with
/// `load_state_from_url` taking `&StateUrl` (not `&str`), a raw user
/// string can never reach the backend layer without going through the
/// parser's scheme + emptiness checks first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateUrl {
    /// `s3://bucket/key` — bucket is the URL host, key is the path
    /// (leading `/` stripped). Region / credentials come from the AWS
    /// SDK default chain at load time.
    #[non_exhaustive]
    S3 { bucket: String, key: String },
    /// `file://path` or a bare local path. The path is taken verbatim
    /// (no normalization) — relative paths resolve against the process
    /// CWD at read time.
    #[non_exhaustive]
    File { path: PathBuf },
}

impl StateUrl {
    /// Classify a `--state-url` argument.
    ///
    /// `s3://` requires a non-empty host and a non-empty key.
    /// `file://` requires a path starting with `/` (RFC 8089
    /// `file:///path` — host-form `file://host/path` is rejected because
    /// only the local-file form is meaningful for state lookup).
    /// Anything else is treated as a bare filesystem path.
    pub fn parse(raw: &str) -> BackendResult<Self> {
        if let Some(rest) = raw.strip_prefix("s3://") {
            let (bucket, key) = rest.split_once('/').ok_or_else(|| {
                BackendError::configuration(format!(
                    "Invalid s3:// URL '{}': expected s3://<bucket>/<key>",
                    raw
                ))
            })?;
            if bucket.is_empty() {
                return Err(BackendError::configuration(format!(
                    "Invalid s3:// URL '{}': bucket is empty",
                    raw
                )));
            }
            if key.is_empty() {
                return Err(BackendError::configuration(format!(
                    "Invalid s3:// URL '{}': key is empty",
                    raw
                )));
            }
            Ok(StateUrl::S3 {
                bucket: bucket.to_string(),
                key: key.to_string(),
            })
        } else if let Some(rest) = raw.strip_prefix("file://") {
            if rest.is_empty() {
                return Err(BackendError::configuration(format!(
                    "Invalid file:// URL '{}': path is empty",
                    raw
                )));
            }
            // Reject `file://host/path` (RFC 8089 host form) and
            // `file://./relative` style inputs — silently treating
            // `file://localhost/foo` as the relative path
            // `localhost/foo` is the kind of bug operators only catch
            // after the lookup returns "No state at localhost/foo".
            // Require the local-file form `file:///abs/path`.
            if !rest.starts_with('/') {
                return Err(BackendError::configuration(format!(
                    "Invalid file:// URL '{}': use file:///<absolute-path> \
                     (host form file://host/path is not supported)",
                    raw
                )));
            }
            Ok(StateUrl::File {
                path: PathBuf::from(rest),
            })
        } else if raw.contains("://") {
            // Reject other URL schemes explicitly rather than silently
            // treating them as bare paths — the user almost certainly
            // meant something else.
            Err(BackendError::configuration(format!(
                "Unsupported URL scheme in '{}': only s3://, file://, or a bare path are accepted",
                raw
            )))
        } else {
            if raw.is_empty() {
                return Err(BackendError::configuration(
                    "Empty --state-url is not allowed",
                ));
            }
            Ok(StateUrl::File {
                path: PathBuf::from(raw),
            })
        }
    }

    /// Human-readable form, suitable for migration-warning display
    /// targets and error messages.
    pub fn display_target(&self) -> String {
        match self {
            StateUrl::S3 { bucket, key } => format!("s3://{}/{}", bucket, key),
            StateUrl::File { path } => path.display().to_string(),
        }
    }
}

/// Load a [`StateFile`] from a parsed `--state-url`.
///
/// Read-only: never acquires a lock, never persists migrations. If the
/// on-disk state needs a schema migration, the lift happens in memory
/// via the underlying backend (`S3Backend` / `LocalBackend`), which
/// emits the per-backend migration warning to stderr through
/// [`crate::state::log_state_migration_once`]. The lifted state is
/// returned but the on-disk file is not rewritten — same shape as the
/// existing read-only `state lookup`/`list`/`show` backend path.
///
/// Returns an error if the URL resolves to a missing object/file.
pub async fn load_state_from_url(url: &StateUrl) -> BackendResult<StateFile> {
    let loaded = match url {
        StateUrl::S3 { bucket, key } => {
            crate::backends::S3Backend::from_url_parts(bucket.clone(), key.clone())
                .await?
                .read_state()
                .await?
        }
        StateUrl::File { path } => {
            crate::backends::LocalBackend::with_path(path.clone())
                .read_state()
                .await?
        }
    };

    loaded
        .map(|l| l.into_state())
        .ok_or_else(|| BackendError::InvalidState(format!("No state at {}", url.display_target())))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- StateUrl::parse ----

    #[test]
    fn parse_s3_url() {
        let url = StateUrl::parse("s3://my-bucket/path/to/state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::S3 {
                bucket: "my-bucket".to_string(),
                key: "path/to/state.json".to_string(),
            }
        );
    }

    #[test]
    fn parse_s3_url_with_single_segment_key() {
        let url = StateUrl::parse("s3://b/state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::S3 {
                bucket: "b".to_string(),
                key: "state.json".to_string(),
            }
        );
    }

    #[test]
    fn parse_s3_url_missing_key_errors() {
        let err = StateUrl::parse("s3://just-a-bucket").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("s3://"), "unexpected error: {msg}");
    }

    #[test]
    fn parse_s3_url_empty_bucket_errors() {
        let err = StateUrl::parse("s3:///key.json").unwrap_err();
        assert!(err.to_string().contains("bucket is empty"));
    }

    #[test]
    fn parse_s3_url_empty_key_errors() {
        let err = StateUrl::parse("s3://bucket/").unwrap_err();
        assert!(err.to_string().contains("key is empty"));
    }

    #[test]
    fn parse_file_url_absolute() {
        let url = StateUrl::parse("file:///tmp/state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::File {
                path: PathBuf::from("/tmp/state.json"),
            }
        );
    }

    #[test]
    fn parse_file_url_empty_path_errors() {
        let err = StateUrl::parse("file://").unwrap_err();
        assert!(err.to_string().contains("path is empty"));
    }

    #[test]
    fn parse_file_url_host_form_rejected() {
        // RFC 8089 `file://host/path` would silently parse to the
        // relative path `host/path` if not rejected — guard against
        // `file://localhost/foo` looking like it worked but actually
        // reading a non-existent `./localhost/foo`.
        let err = StateUrl::parse("file://localhost/tmp/state.json").unwrap_err();
        assert!(
            err.to_string().contains("file:///"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_file_url_relative_after_scheme_rejected() {
        let err = StateUrl::parse("file://./carina.state.json").unwrap_err();
        assert!(
            err.to_string().contains("file:///"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn parse_bare_relative_path() {
        let url = StateUrl::parse("./carina.state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::File {
                path: PathBuf::from("./carina.state.json"),
            }
        );
    }

    #[test]
    fn parse_bare_absolute_path() {
        let url = StateUrl::parse("/var/state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::File {
                path: PathBuf::from("/var/state.json"),
            }
        );
    }

    #[test]
    fn parse_bare_simple_name() {
        let url = StateUrl::parse("state.json").unwrap();
        assert_eq!(
            url,
            StateUrl::File {
                path: PathBuf::from("state.json"),
            }
        );
    }

    #[test]
    fn parse_https_scheme_is_rejected() {
        let err = StateUrl::parse("https://example.com/state.json").unwrap_err();
        assert!(err.to_string().contains("Unsupported URL scheme"));
    }

    #[test]
    fn parse_gcs_scheme_is_rejected() {
        let err = StateUrl::parse("gs://bucket/key.json").unwrap_err();
        assert!(err.to_string().contains("Unsupported URL scheme"));
    }

    #[test]
    fn parse_empty_string_errors() {
        let err = StateUrl::parse("").unwrap_err();
        assert!(err.to_string().contains("Empty"));
    }

    // ---- display_target ----

    #[test]
    fn display_target_s3() {
        let url = StateUrl::parse("s3://b/k.json").unwrap();
        assert_eq!(url.display_target(), "s3://b/k.json");
    }

    #[test]
    fn display_target_file() {
        let url = StateUrl::parse("/tmp/state.json").unwrap();
        assert_eq!(url.display_target(), "/tmp/state.json");
    }

    // ---- load_state_from_url (file path) ----

    #[tokio::test]
    async fn load_local_file_url_returns_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("carina.state.json");
        let mut state = StateFile::new();
        state.increment_serial();
        let bytes = serde_json::to_vec_pretty(&state).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let url = StateUrl::parse(path.to_str().unwrap()).unwrap();
        let loaded = load_state_from_url(&url).await.unwrap();
        assert_eq!(loaded.serial, state.serial);
    }

    #[tokio::test]
    async fn load_file_url_with_scheme_returns_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        let state = StateFile::new();
        let bytes = serde_json::to_vec_pretty(&state).unwrap();
        std::fs::write(&path, &bytes).unwrap();

        let url = StateUrl::parse(&format!("file://{}", path.display())).unwrap();
        let loaded = load_state_from_url(&url).await.unwrap();
        assert_eq!(loaded.serial, state.serial);
    }

    #[tokio::test]
    async fn load_missing_local_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let url = StateUrl::parse(path.to_str().unwrap()).unwrap();
        let err = load_state_from_url(&url).await.unwrap_err();
        assert!(
            err.to_string().contains("No state"),
            "unexpected error: {err}"
        );
    }
}
