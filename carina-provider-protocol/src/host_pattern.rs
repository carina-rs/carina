use std::error::Error;
use std::fmt;
use std::net::IpAddr;

const METADATA_HOSTNAMES: &[&str] = &[
    "instance-data.ec2.internal",
    "metadata.azure.internal",
    "metadata.google.internal",
];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HostPattern(HostPatternKind);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum HostPatternKind {
    Exact(AsciiHost),
    OneLabel(RegistrableSuffix),
    AnyDepth(RegistrableSuffix),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AsciiHost(String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RegistrableSuffix(String);

impl HostPattern {
    pub fn parse(pattern: &str) -> Result<Self, HostPatternError> {
        if !pattern.is_ascii() {
            return Err(HostPatternError::NonAscii);
        }
        let normalized = strip_one_trailing_dot(pattern).to_ascii_lowercase();
        let kind = if let Some(literal) = normalized.strip_prefix("**.") {
            validate_pattern_literal(literal)?;
            reject_public_suffix(literal)?;
            HostPatternKind::AnyDepth(RegistrableSuffix(literal.to_string()))
        } else if let Some(literal) = normalized.strip_prefix("*.") {
            validate_pattern_literal(literal)?;
            reject_public_suffix(literal)?;
            HostPatternKind::OneLabel(RegistrableSuffix(literal.to_string()))
        } else {
            validate_pattern_literal(&normalized)?;
            HostPatternKind::Exact(AsciiHost(normalized))
        };
        Ok(Self(kind))
    }

    pub fn matches(&self, host: &str) -> bool {
        let Ok(host) = normalize_request_host(host) else {
            return false;
        };
        match &self.0 {
            HostPatternKind::Exact(AsciiHost(expected)) => host == *expected,
            HostPatternKind::OneLabel(RegistrableSuffix(suffix)) => {
                wildcard_prefix(&host, suffix).is_some_and(|prefix| !prefix.contains('.'))
            }
            HostPatternKind::AnyDepth(RegistrableSuffix(suffix)) => {
                wildcard_prefix(&host, suffix).is_some()
            }
        }
    }

    pub fn is_any_depth(&self) -> bool {
        matches!(self.0, HostPatternKind::AnyDepth(_))
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            HostPatternKind::Exact(AsciiHost(host)) => host.fmt(formatter),
            HostPatternKind::OneLabel(RegistrableSuffix(suffix)) => {
                write!(formatter, "*.{suffix}")
            }
            HostPatternKind::AnyDepth(RegistrableSuffix(suffix)) => {
                write!(formatter, "**.{suffix}")
            }
        }
    }
}

fn strip_one_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

fn ip_literal(host: &str) -> Option<IpAddr> {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
        .parse()
        .ok()
}

fn validate_pattern_literal(host: &str) -> Result<(), HostPatternError> {
    if host.contains('*') {
        return Err(HostPatternError::InvalidWildcard);
    }
    validate_literal_host(host)
}

fn validate_literal_host(host: &str) -> Result<(), HostPatternError> {
    if host.is_empty() {
        return Err(HostPatternError::Empty);
    }
    if ip_literal(host).is_some() {
        return Err(HostPatternError::IpLiteral);
    }
    if ends_in_number(host) {
        return Err(HostPatternError::IpLiteral);
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(HostPatternError::EmptyLabel);
        }
        if label.len() > 63 {
            return Err(HostPatternError::LabelTooLong);
        }
        if label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(HostPatternError::InvalidLabel);
        }
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return Err(HostPatternError::Localhost);
    }
    if host == "local" || host.ends_with(".local") || METADATA_HOSTNAMES.contains(&host) {
        return Err(HostPatternError::ReservedName);
    }
    Ok(())
}

fn ends_in_number(host: &str) -> bool {
    let last = host.rsplit('.').next().unwrap_or(host).as_bytes();
    !last.is_empty()
        && (last.iter().all(u8::is_ascii_digit)
            || (last.len() > 2
                && last[0] == b'0'
                && matches!(last[1], b'x' | b'X')
                && last[2..].iter().all(u8::is_ascii_hexdigit)))
}

fn reject_public_suffix(suffix: &str) -> Result<(), HostPatternError> {
    if psl::suffix(suffix.as_bytes())
        .is_some_and(|public_suffix| public_suffix == suffix.as_bytes())
    {
        return Err(HostPatternError::PublicSuffix);
    }
    Ok(())
}

fn normalize_request_host(host: &str) -> Result<String, HostPatternError> {
    if !host.is_ascii() {
        return Err(HostPatternError::NonAscii);
    }
    let host = strip_one_trailing_dot(host).to_ascii_lowercase();
    if host.contains('*') {
        return Err(HostPatternError::InvalidWildcard);
    }
    validate_literal_host(&host)?;
    Ok(host)
}

fn wildcard_prefix<'a>(host: &'a str, suffix: &str) -> Option<&'a str> {
    host.strip_suffix(suffix)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .filter(|prefix| !prefix.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPatternError {
    Empty,
    NonAscii,
    InvalidWildcard,
    EmptyLabel,
    LabelTooLong,
    InvalidLabel,
    IpLiteral,
    Localhost,
    ReservedName,
    PublicSuffix,
}

impl fmt::Display for HostPatternError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the host is empty"),
            Self::NonAscii => formatter.write_str("host labels must be ASCII A-labels"),
            Self::InvalidWildcard => {
                formatter.write_str("a wildcard must be the whole leftmost label")
            }
            Self::EmptyLabel => formatter.write_str("host labels must not be empty"),
            Self::LabelTooLong => formatter.write_str("host labels must not exceed 63 bytes"),
            Self::InvalidLabel => formatter.write_str(
                "host labels may contain only ASCII letters, digits, and hyphens, and must not start or end with a hyphen",
            ),
            Self::IpLiteral => formatter.write_str("IP literals are not allowed"),
            Self::Localhost => formatter.write_str("localhost names are not allowed"),
            Self::ReservedName => {
                formatter.write_str("metadata and link-local names are not allowed")
            }
            Self::PublicSuffix => {
                formatter.write_str("a wildcard cannot sit directly above a public suffix")
            }
        }
    }
}

impl Error for HostPatternError {}

#[cfg(test)]
mod tests {
    use super::{HostPattern, normalize_request_host};

    #[test]
    fn normalizes_and_matches_exact_hosts() {
        let pattern = HostPattern::parse("API.Example.COM.").unwrap();
        assert_eq!(pattern.to_string(), "api.example.com");
        assert!(pattern.matches("API.EXAMPLE.COM."));
        assert!(!pattern.matches("x.api.example.com"));
    }

    #[test]
    fn wildcards_have_distinct_depth_semantics() {
        let one = HostPattern::parse("*.a.com").unwrap();
        let any = HostPattern::parse("**.a.com").unwrap();

        assert!(!one.is_any_depth());
        assert!(any.is_any_depth());
        assert!(one.matches("x.a.com"));
        assert!(!one.matches("y.x.a.com"));
        assert!(!one.matches("a.com"));
        assert!(any.matches("x.a.com"));
        assert!(any.matches("y.x.a.com"));
        assert!(!any.matches("a.com"));
    }

    #[test]
    fn rejects_invalid_wildcard_positions() {
        for pattern in ["api.*.com", "*foo.com", "a.**.com", "***.example.com"] {
            assert!(HostPattern::parse(pattern).is_err(), "accepted {pattern}");
        }
    }

    #[test]
    fn rejects_wildcards_directly_above_public_suffixes() {
        for pattern in ["*.com", "**.co.uk", "*.github.io"] {
            assert!(HostPattern::parse(pattern).is_err(), "accepted {pattern}");
        }
    }

    #[test]
    fn rejects_ip_literals_localhost_and_link_local_names() {
        for pattern in [
            "127.0.0.1",
            "[::1]",
            "::1",
            "*.127.0.0.1",
            "localhost",
            "*.localhost",
            "foo.localhost",
            "printer.local",
            "metadata.google.internal",
            "instance-data.ec2.internal",
            "169.254.169.254",
            "[fe80::1]",
        ] {
            assert!(HostPattern::parse(pattern).is_err(), "accepted {pattern}");
        }
    }

    #[test]
    fn rejects_noncanonical_ip_literals() {
        for pattern in ["127.1", "2130706433", "0x7f.1", "0177.0.0.1"] {
            assert!(HostPattern::parse(pattern).is_err(), "accepted {pattern}");
            assert!(
                normalize_request_host(pattern).is_err(),
                "normalized request host {pattern}"
            );
        }
    }

    #[test]
    fn enforces_ldh_labels_for_patterns_and_request_hosts() {
        for host in ["a_b.com", "x\"y.com", "host:443", "-a.com", "a-.com"] {
            assert!(HostPattern::parse(host).is_err(), "accepted {host}");
            assert!(
                normalize_request_host(host).is_err(),
                "normalized request host {host}"
            );
        }
        assert!(HostPattern::parse("a-b.com").is_ok());
        assert_eq!(normalize_request_host("A-B.COM.").unwrap(), "a-b.com");
    }

    #[test]
    fn rejects_non_ascii_empty_and_oversized_labels() {
        let oversized = format!("{}.example.com", "a".repeat(64));
        for pattern in [
            "éxample.com",
            ".example.com",
            "example..com",
            "example.com..",
            oversized.as_str(),
        ] {
            assert!(HostPattern::parse(pattern).is_err(), "accepted {pattern}");
        }
    }

    #[test]
    fn denies_invalid_request_hosts() {
        let pattern = HostPattern::parse("**.example.com").unwrap();
        for host in [
            "é.example.com",
            "x..example.com",
            "127.0.0.1",
            "example.com",
        ] {
            assert!(!pattern.matches(host), "matched {host}");
        }
    }
}
