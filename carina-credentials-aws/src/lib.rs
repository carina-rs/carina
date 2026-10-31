//! AWS SDK default-chain implementation of Carina's host credential source.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use aws_credential_types::provider::error::CredentialsError;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use carina_plugin_host::{AwsCredentialError, AwsCredentialSource, AwsCredentials};
use tokio::sync::Mutex;

const REFRESH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// A run-scoped, refreshing AWS SDK default credential chain.
pub struct AwsSdkCredentialSource {
    provider: Option<SharedCredentialsProvider>,
    cache: Mutex<Option<AwsCredentials>>,
}

impl AwsSdkCredentialSource {
    /// Loads the AWS SDK default chain, including environment, profiles, SSO,
    /// ECS, and IMDS providers.
    pub async fn new() -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .identity_cache(aws_config::identity::IdentityCache::lazy().build())
            .load()
            .await;
        Self {
            provider: config.credentials_provider(),
            cache: Mutex::new(None),
        }
    }

    /// Returns the cached credentials, refreshing the selected SDK provider
    /// when the credentials are close to expiry.
    pub async fn current_credentials(&self) -> Result<AwsCredentials, AwsCredentialError> {
        let mut cache = self.cache.lock().await;
        if let Some(credentials) = cache.as_ref()
            && credentials_are_fresh(credentials, SystemTime::now())
        {
            return Ok(credentials.clone());
        }

        let provider = self.provider.as_ref().ok_or_else(|| {
            AwsCredentialError::NotFound(
                "the AWS SDK default chain did not configure a credential provider".to_string(),
            )
        })?;
        let credentials = provider
            .provide_credentials()
            .await
            .map_err(map_credentials_error)?;
        let credentials = AwsCredentials::new(
            credentials.access_key_id(),
            credentials.secret_access_key(),
            credentials.session_token().map(str::to_string),
            credentials.expiry(),
        );
        *cache = Some(credentials.clone());
        Ok(credentials)
    }

    #[cfg(test)]
    fn from_provider_for_test(provider: impl ProvideCredentials + 'static) -> Self {
        Self {
            provider: Some(SharedCredentialsProvider::new(provider)),
            cache: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn from_static_for_test(
        access_key_id: &str,
        secret_access_key: &str,
        session_token: Option<&str>,
        expires_at: Option<SystemTime>,
    ) -> Self {
        Self::from_provider_for_test(aws_credential_types::Credentials::new(
            access_key_id,
            secret_access_key,
            session_token.map(str::to_string),
            expires_at,
            "carina-test",
        ))
    }
}

impl fmt::Debug for AwsSdkCredentialSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AwsSdkCredentialSource")
            .field("provider", &self.provider.as_ref().map(|_| "<configured>"))
            .field("cache", &"<redacted>")
            .finish()
    }
}

impl AwsCredentialSource for AwsSdkCredentialSource {
    fn current(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<AwsCredentials, AwsCredentialError>> + Send + '_>> {
        Box::pin(self.current_credentials())
    }
}

fn credentials_are_fresh(credentials: &AwsCredentials, now: SystemTime) -> bool {
    credentials.expires_at().is_none_or(|expires_at| {
        expires_at
            .duration_since(now)
            .is_ok_and(|remaining| remaining > REFRESH_WINDOW)
    })
}

fn map_credentials_error(error: CredentialsError) -> AwsCredentialError {
    let not_found = matches!(error, CredentialsError::CredentialsNotLoaded(_));
    let mut message = render_error_chain(&error);
    if message
        .to_ascii_lowercase()
        .contains("sso token has expired")
        && !message.contains("aws sso login")
    {
        message.push_str("; run `aws sso login` to refresh the session");
    }
    if not_found {
        AwsCredentialError::NotFound(message)
    } else {
        AwsCredentialError::ProviderFailed(message)
    }
}

fn render_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        rendered.push_str(": ");
        rendered.push_str(&error.to_string());
        source = error.source();
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::ffi::{OsStr, OsString};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    use aws_credential_types::Credentials;
    use aws_credential_types::provider::future;
    use sha1::{Digest, Sha1};

    use super::*;

    #[tokio::test]
    async fn static_provider_returns_credentials_without_exposing_secrets() {
        let source = AwsSdkCredentialSource::from_static_for_test(
            "AKIASTATIC",
            "static-secret-sentinel",
            Some("static-session-sentinel"),
            None,
        );

        let credentials = source.current_credentials().await.unwrap();
        assert_eq!(credentials.access_key_id(), "AKIASTATIC");
        assert_eq!(credentials.secret_access_key(), "static-secret-sentinel");
        assert_eq!(credentials.session_token(), Some("static-session-sentinel"));
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("static-secret-sentinel"));
        assert!(!rendered.contains("static-session-sentinel"));
        let source_debug = format!("{source:?}");
        assert!(!source_debug.contains("static-secret-sentinel"));
        assert!(!source_debug.contains("static-session-sentinel"));
    }

    struct SequenceProvider {
        calls: Arc<AtomicUsize>,
        credentials: StdMutex<VecDeque<Credentials>>,
    }

    impl fmt::Debug for SequenceProvider {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("SequenceProvider(<redacted>)")
        }
    }

    impl ProvideCredentials for SequenceProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let credentials = self
                .credentials
                .lock()
                .unwrap()
                .pop_front()
                .expect("test provider must have one credential per refresh");
            future::ProvideCredentials::ready(Ok(credentials))
        }
    }

    #[tokio::test]
    async fn current_caches_and_refreshes_credentials_near_expiry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = SequenceProvider {
            calls: Arc::clone(&calls),
            credentials: StdMutex::new(VecDeque::from([
                Credentials::new(
                    "AKIAEXPIRING",
                    "first-secret",
                    None,
                    Some(SystemTime::now() + Duration::from_secs(60)),
                    "test",
                ),
                Credentials::new("AKIAFRESH", "second-secret", None, None, "test"),
            ])),
        };
        let source = AwsSdkCredentialSource::from_provider_for_test(provider);

        let first = source.current_credentials().await.unwrap();
        let second = source.current_credentials().await.unwrap();

        assert_eq!(first.access_key_id(), "AKIAEXPIRING");
        assert_eq!(second.access_key_id(), "AKIAFRESH");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[derive(Debug)]
    struct FailingProvider;

    impl ProvideCredentials for FailingProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Err(CredentialsError::provider_error(
                "credential provider test failure",
            )))
        }
    }

    #[tokio::test]
    async fn provider_errors_remain_typed_and_secret_free() {
        let source = AwsSdkCredentialSource::from_provider_for_test(FailingProvider);
        let error = source.current_credentials().await.unwrap_err();

        assert!(matches!(error, AwsCredentialError::ProviderFailed(_)));
        let rendered = error.to_string();
        assert!(rendered.contains("credential provider test failure"));
        assert!(!rendered.contains("static-secret-sentinel"));
        assert!(!rendered.contains("static-session-sentinel"));
    }

    #[derive(Debug)]
    struct MissingProvider;

    impl ProvideCredentials for MissingProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Err(CredentialsError::not_loaded_no_source()))
        }
    }

    #[tokio::test]
    async fn missing_chain_maps_to_not_found() {
        let source = AwsSdkCredentialSource::from_provider_for_test(MissingProvider);
        assert!(matches!(
            source.current_credentials().await,
            Err(AwsCredentialError::NotFound(_))
        ));
    }

    #[test]
    fn sdk_expired_sso_message_gets_a_noninteractive_login_hint() {
        let error =
            CredentialsError::provider_error("the SSO token has expired and cannot be refreshed");
        let mapped = map_credentials_error(error);
        assert!(matches!(mapped, AwsCredentialError::ProviderFailed(_)));
        assert!(mapped.to_string().contains("aws sso login"));
    }

    static ENVIRONMENT_LOCK: StdMutex<()> = StdMutex::new(());

    struct EnvironmentGuard {
        previous: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn isolated(home: &Path, profile: Option<&str>) -> Self {
            const NAMES: &[&str] = &[
                "HOME",
                "USERPROFILE",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_SECURITY_TOKEN",
                "AWS_PROFILE",
                "AWS_DEFAULT_PROFILE",
                "AWS_WEB_IDENTITY_TOKEN_FILE",
                "AWS_ROLE_ARN",
                "AWS_ROLE_SESSION_NAME",
                "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
                "AWS_CONTAINER_CREDENTIALS_FULL_URI",
                "AWS_CONTAINER_AUTHORIZATION_TOKEN",
                "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
                "AWS_CONFIG_FILE",
                "AWS_SHARED_CREDENTIALS_FILE",
                "AWS_EC2_METADATA_DISABLED",
                "AWS_SDK_LOAD_CONFIG",
                "AWS_ENDPOINT_URL_SSO_OIDC",
                "SSL_CERT_FILE",
                "SSL_CERT_DIR",
            ];
            let current = std::env::vars_os().collect::<Vec<_>>();
            let previous = NAMES
                .iter()
                .map(|name| {
                    let value = current
                        .iter()
                        .find(|(key, _)| key == OsStr::new(name))
                        .map(|(_, value)| value.clone());
                    (*name, value)
                })
                .collect();

            for name in NAMES {
                // SAFETY: every environment-mutating test in this test binary
                // holds ENVIRONMENT_LOCK for the guard's full lifetime.
                unsafe { std::env::remove_var(name) };
            }
            let config = home.join(".aws/config");
            let credentials = home.join(".aws/credentials");
            for (name, value) in [
                ("HOME", home.as_os_str()),
                ("USERPROFILE", home.as_os_str()),
                ("AWS_CONFIG_FILE", config.as_os_str()),
                ("AWS_SHARED_CREDENTIALS_FILE", credentials.as_os_str()),
                ("AWS_EC2_METADATA_DISABLED", OsStr::new("true")),
                ("AWS_SDK_LOAD_CONFIG", OsStr::new("true")),
                (
                    "AWS_ENDPOINT_URL_SSO_OIDC",
                    OsStr::new("http://127.0.0.1:9"),
                ),
                ("SSL_CERT_FILE", OsStr::new("/etc/ssl/cert.pem")),
            ] {
                // SAFETY: see the serialized mutation above.
                unsafe { std::env::set_var(name, value) };
            }
            if let Some(profile) = profile {
                // SAFETY: see the serialized mutation above.
                unsafe { std::env::set_var("AWS_PROFILE", profile) };
            }

            Self { previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            while let Some((name, previous)) = self.previous.pop() {
                match previous {
                    Some(value) => {
                        // SAFETY: ENVIRONMENT_LOCK remains held until after
                        // this guard is dropped.
                        unsafe { std::env::set_var(name, value) };
                    }
                    None => {
                        // SAFETY: ENVIRONMENT_LOCK remains held until after
                        // this guard is dropped.
                        unsafe { std::env::remove_var(name) };
                    }
                }
            }
        }
    }

    fn write_aws_files(home: &Path, config: &str) {
        let aws_dir = home.join(".aws");
        std::fs::create_dir_all(&aws_dir).unwrap();
        std::fs::write(aws_dir.join("config"), config).unwrap();
        std::fs::write(aws_dir.join("credentials"), "").unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_environment_returns_not_found_within_ten_seconds() {
        let _lock = ENVIRONMENT_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_aws_files(home.path(), "");
        let _environment = EnvironmentGuard::isolated(home.path(), None);

        let result = tokio::time::timeout(Duration::from_secs(10), async {
            AwsSdkCredentialSource::new()
                .await
                .current_credentials()
                .await
        })
        .await
        .expect("an empty, IMDS-disabled chain must not hang");

        assert!(matches!(result, Err(AwsCredentialError::NotFound(_))));
    }

    fn cache_filename(identifier: &str) -> PathBuf {
        let digest = Sha1::digest(identifier.as_bytes());
        let hex = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        PathBuf::from(format!("{hex}.json"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_nonrefreshable_sso_session_fails_promptly_without_login() {
        let _lock = ENVIRONMENT_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let start_url = "https://d-example.awsapps.com/start";
        let session_name = "expired-session";
        write_aws_files(
            home.path(),
            &format!(
                "[profile expired-sso]\n\
                 sso_session = {session_name}\n\
                 sso_account_id = 123456789012\n\
                 sso_role_name = ReadOnly\n\
                 region = us-east-1\n\n\
                 [sso-session {session_name}]\n\
                 sso_start_url = {start_url}\n\
                 sso_region = us-east-1\n"
            ),
        );
        let cache_dir = home.path().join(".aws/sso/cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let token = format!(
            "{{\"startUrl\":\"{start_url}\",\"region\":\"us-east-1\",\
             \"accessToken\":\"expired-access-token-secret\",\
             \"expiresAt\":\"2000-01-01T00:00:00Z\"}}"
        );
        for identifier in [session_name, start_url] {
            std::fs::write(cache_dir.join(cache_filename(identifier)), &token).unwrap();
        }
        let _environment = EnvironmentGuard::isolated(home.path(), Some("expired-sso"));

        let error = tokio::time::timeout(Duration::from_secs(10), async {
            AwsSdkCredentialSource::new()
                .await
                .current_credentials()
                .await
        })
        .await
        .expect("an expired, non-refreshable SSO session must not hang")
        .expect_err("expired SSO must fail instead of starting interactive login");

        assert!(matches!(error, AwsCredentialError::ProviderFailed(_)));
        let rendered = error.to_string();
        assert!(
            rendered.contains("aws sso login"),
            "unexpected error: {rendered}"
        );
        assert!(!rendered.contains("expired-access-token-secret"));
    }
}
