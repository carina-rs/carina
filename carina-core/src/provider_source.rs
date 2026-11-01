use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Registry hostname used when a source omits its hostname.
pub const DEFAULT_REGISTRY_HOST: &str = "registry.carina-rs.dev";

/// Coordinates for a provider downloaded directly from GitHub releases.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubSource {
    owner: String,
    repo: String,
}

impl GithubSource {
    /// Return the GitHub repository owner.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Return the GitHub repository name.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Return the typed canonical key used by provider locks and caches.
    pub fn source_key(&self) -> RemoteSourceKey {
        RemoteSourceKey {
            value: self.to_string(),
            artifact_name: self.repo.clone(),
        }
    }
}

impl fmt::Display for GithubSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "github.com/{}/{}", self.owner, self.repo)
    }
}

/// Parsed registry coordinates for a provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RegistrySource {
    hostname: String,
    namespace: String,
    name: String,
}

impl RegistrySource {
    /// Return the registry hostname, normalized to lowercase by the parser.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Return the provider namespace within the registry.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Return the provider name within the namespace.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Return the typed canonical key used by provider locks and caches.
    pub fn source_key(&self) -> RemoteSourceKey {
        RemoteSourceKey {
            value: self.canonical_key(),
            artifact_name: self.name.clone(),
        }
    }

    fn canonical_key(&self) -> String {
        if self.hostname == DEFAULT_REGISTRY_HOST {
            format!("{}/{}", self.namespace, self.name)
        } else {
            format!("{}/{}/{}", self.hostname, self.namespace, self.name)
        }
    }
}

impl fmt::Display for RegistrySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical_key())
    }
}

/// A validated provider source that is resolved over the network.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RemoteSource {
    /// A provider downloaded directly from GitHub releases.
    GithubDirect(GithubSource),
    /// A provider resolved through a Carina registry.
    Registry(RegistrySource),
}

impl RemoteSource {
    /// Return the typed canonical key used by provider locks and caches.
    pub fn source_key(&self) -> RemoteSourceKey {
        match self {
            Self::GithubDirect(source) => source.source_key(),
            Self::Registry(source) => source.source_key(),
        }
    }
}

/// Canonical lock/cache identity derived from a validated remote source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteSourceKey {
    value: String,
    artifact_name: String,
}

impl RemoteSourceKey {
    /// Return the canonical string stored in lock files and cache directories.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Return the provider component filename stem for this source.
    pub fn artifact_name(&self) -> &str {
        &self.artifact_name
    }
}

impl fmt::Display for RemoteSourceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.value)
    }
}

impl fmt::Display for RemoteSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GithubDirect(source) => source.fmt(f),
            Self::Registry(source) => source.fmt(f),
        }
    }
}

/// A validated local provider source.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FileSource {
    path: String,
}

impl FileSource {
    /// Return the local component path following the `file://` prefix.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// A validated provider source accepted by Carina configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderSource {
    /// A local WebAssembly component selected through a `file://` URI.
    File(FileSource),
    /// A provider resolved from a remote source.
    Remote(RemoteSource),
}

/// Render an untrusted spelling without allowing terminal control characters
/// or delimiters to affect the surrounding diagnostic.
pub fn display_untrusted(value: &str) -> impl fmt::Display + '_ {
    struct Escaped<'a>(&'a str);

    impl fmt::Display for Escaped<'_> {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.escape_debug().fmt(formatter)
        }
    }

    Escaped(value)
}

/// Error returned when a provider source string is unsafe or has an unsupported shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSourceParseError {
    /// The source does not match any supported provider source form.
    InvalidFormat { input: String },
    /// A local provider path contains a terminal control character.
    FilePathContainsControl { input: String },
}

impl fmt::Display for ProviderSourceParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidFormat { input } => write!(
                formatter,
                "Invalid source format: {}. Expected: file://<path>, github.com/{{owner}}/{{repo}}, or [hostname/]namespace/name",
                display_untrusted(input)
            ),
            Self::FilePathContainsControl { input } => write!(
                formatter,
                "Invalid file provider source \"{}\": file:// paths must not contain control characters",
                display_untrusted(input)
            ),
        }
    }
}

impl std::error::Error for ProviderSourceParseError {}

fn invalid_source(source: &str) -> ProviderSourceParseError {
    ProviderSourceParseError::InvalidFormat {
        input: source.to_string(),
    }
}

fn valid_name_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn normalize_hostname(hostname: &str) -> Option<String> {
    let hostname = hostname.to_ascii_lowercase();
    let (host, port) = match hostname.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':')
                || port.is_empty()
                || !port.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            (host, Some(port))
        }
        None => (hostname.as_str(), None),
    };

    if host == "."
        || host == ".."
        || host.split('.').any(|label| {
            label.is_empty()
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return None;
    }

    Some(match port {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

impl FromStr for ProviderSource {
    type Err = ProviderSourceParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        if let Some(path) = source.strip_prefix("file://") {
            return if path.chars().any(char::is_control) {
                Err(ProviderSourceParseError::FilePathContainsControl {
                    input: source.to_string(),
                })
            } else if path.is_empty() {
                Err(invalid_source(source))
            } else {
                Ok(Self::File(FileSource {
                    path: path.to_string(),
                }))
            };
        }

        let parts: Vec<&str> = source.split('/').collect();
        let first_segment = parts.first().map(|segment| segment.to_ascii_lowercase());
        match parts.as_slice() {
            [_, owner, repo]
                if first_segment.as_deref() == Some("github.com")
                    && valid_name_segment(owner)
                    && valid_name_segment(repo) =>
            {
                Ok(Self::Remote(RemoteSource::GithubDirect(GithubSource {
                    owner: (*owner).to_string(),
                    repo: (*repo).to_string(),
                })))
            }
            [namespace, name]
                if !namespace.eq_ignore_ascii_case("github.com")
                    && valid_name_segment(namespace)
                    && valid_name_segment(name) =>
            {
                Ok(Self::Remote(RemoteSource::Registry(RegistrySource {
                    hostname: DEFAULT_REGISTRY_HOST.to_string(),
                    namespace: (*namespace).to_string(),
                    name: (*name).to_string(),
                })))
            }
            [hostname, namespace, name]
                if !hostname
                    .split_once(':')
                    .map_or(*hostname, |(host, _)| host)
                    .eq_ignore_ascii_case("github.com")
                    && valid_name_segment(namespace)
                    && valid_name_segment(name) =>
            {
                let hostname =
                    normalize_hostname(hostname).ok_or_else(|| invalid_source(source))?;
                Ok(Self::Remote(RemoteSource::Registry(RegistrySource {
                    hostname,
                    namespace: (*namespace).to_string(),
                    name: (*name).to_string(),
                })))
            }
            _ => Err(invalid_source(source)),
        }
    }
}

impl fmt::Display for ProviderSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(source) => write!(f, "file://{}", source.path()),
            Self::Remote(source) => source.fmt(f),
        }
    }
}

impl Serialize for ProviderSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ProviderSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let source = String::deserialize(deserializer)?;
        source.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_REGISTRY_HOST, ProviderSource, ProviderSourceParseError, RemoteSource};

    #[test]
    fn parsed_sources_expose_validated_coordinates() {
        match "file:///x.wasm".parse::<ProviderSource>().unwrap() {
            ProviderSource::File(source) => assert_eq!(source.path(), "/x.wasm"),
            source => panic!("expected file source: {source}"),
        }

        match "github.com/o/r".parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(RemoteSource::GithubDirect(source)) => {
                assert_eq!(source.owner(), "o");
                assert_eq!(source.repo(), "r");
                assert_eq!(source.source_key().as_str(), "github.com/o/r");
            }
            source => panic!("expected GitHub source: {source}"),
        }

        match "ns/name".parse::<ProviderSource>().unwrap() {
            ProviderSource::Remote(RemoteSource::Registry(source)) => {
                assert_eq!(source.hostname(), DEFAULT_REGISTRY_HOST);
                assert_eq!(source.namespace(), "ns");
                assert_eq!(source.name(), "name");
                assert_eq!(source.source_key().as_str(), "ns/name");
            }
            source => panic!("expected registry source: {source}"),
        }
    }

    #[test]
    fn github_host_dispatch_is_ascii_case_insensitive() {
        let parsed = "GitHub.com/o/r".parse::<ProviderSource>().unwrap();

        assert_eq!(parsed, "github.com/o/r".parse::<ProviderSource>().unwrap());
        assert_eq!(parsed.to_string(), "github.com/o/r");
        assert_eq!(
            parsed.to_string().parse::<ProviderSource>().unwrap(),
            parsed
        );
    }

    #[test]
    fn rejects_invalid_provider_source_domains() {
        for source in [
            "file://",
            "github.com//repo",
            "github.com/owner/",
            "github.com/carina-rs",
            "GitHub.com/carina-rs",
            "github.com:443/o/r",
            "GitHub.com:443/o/r",
            "a//b",
            "../../..",
            "namespace/..",
            "namespace/.",
            "namespace/has space",
            "namespace/name?query",
            "namespace/name#fragment",
            "exa mple.com/ns/name",
            "example_com/ns/name",
            ".example.com/ns/name",
            "example..com/ns/name",
            "example.com:/ns/name",
            "example.com:not-a-port/ns/name",
        ] {
            let error = source
                .parse::<ProviderSource>()
                .expect_err("invalid provider source must be rejected");
            assert!(
                error.to_string().contains(
                    "Expected: file://<path>, github.com/{owner}/{repo}, or [hostname/]namespace/name"
                ),
                "{source}: {error}"
            );
        }
    }

    #[test]
    fn file_sources_reject_control_characters_without_rendering_them_raw() {
        for control in ['\u{1b}', '\u{7}', '\r', '\n', '\0'] {
            let source = format!("file://provider{control}.wasm");
            let error = source
                .parse::<ProviderSource>()
                .expect_err("file provider control characters must be rejected");

            assert!(matches!(
                error,
                ProviderSourceParseError::FilePathContainsControl { .. }
            ));
            let rendered = error.to_string();
            assert!(
                rendered.contains("file:// paths must not contain control characters"),
                "{rendered}"
            );
            assert!(
                !rendered.chars().any(char::is_control),
                "error rendered a control character: {rendered:?}"
            );
        }

        assert!("file://provider.wasm".parse::<ProviderSource>().is_ok());
    }

    #[test]
    fn invalid_source_format_does_not_render_control_characters_raw() {
        let source = "not-a-source\u{1b}\u{7}";
        let rendered = source
            .parse::<ProviderSource>()
            .expect_err("the fixture must have an invalid source shape")
            .to_string();

        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(rendered.contains(r"\u{1b}"), "{rendered:?}");
        assert!(rendered.contains(r"\u{7}"), "{rendered:?}");
    }

    #[test]
    fn parse_display_parse_round_trips_all_source_variants() {
        for (input, expected_display) in [
            ("file:///x.wasm", "file:///x.wasm"),
            ("github.com/o/r", "github.com/o/r"),
            ("carina-rs/awscc", "carina-rs/awscc"),
            (
                "REGISTRY.EXAMPLE.COM/ns/name",
                "registry.example.com/ns/name",
            ),
            (
                "REGISTRY.EXAMPLE.COM:8443/ns/name",
                "registry.example.com:8443/ns/name",
            ),
        ] {
            let parsed = input.parse::<ProviderSource>().unwrap();
            let displayed = parsed.to_string();
            let reparsed = displayed.parse::<ProviderSource>().unwrap();

            assert_eq!(displayed, expected_display);
            assert_eq!(reparsed, parsed);
        }
    }
}
