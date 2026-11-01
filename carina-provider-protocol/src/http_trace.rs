//! Request rendering for HTTP traces shared by the guest SDK and host.
//!
//! Only explicitly allowed header values are visible. URI userinfo and
//! fragments are omitted and every query value is redacted. This module uses only
//! the standard library.

use std::fmt;

const REDACTED: &str = "<redacted>";

/// A request header list whose values are redacted unless explicitly allowed.
#[derive(Clone, Copy)]
pub struct TraceHeaders<'a>(&'a [(String, Vec<u8>)]);

impl<'a> TraceHeaders<'a> {
    /// Creates a trace view with header values redacted unless allowlisted.
    pub fn new(headers: &'a [(String, Vec<u8>)]) -> Self {
        Self(headers)
    }
}

impl fmt::Display for TraceHeaders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// Allowlist of header names whose values are never credentials; every other value is redacted.
        const ALLOWED: &[&str] = &[
            "host",
            "content-type",
            "content-length",
            "transfer-encoding",
            "expect",
            "user-agent",
            "x-amz-date",
        ];

        for (index, (name, value)) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            write!(f, "{name}=")?;
            if ALLOWED
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
            {
                write!(f, "{}", String::from_utf8_lossy(value))?;
            } else {
                f.write_str(REDACTED)?;
            }
        }
        Ok(())
    }
}

/// A URI that displays scheme, host, port, the path as-is, and query parameter names.
#[derive(Clone, Copy)]
pub struct TraceUri<'a>(&'a str);

impl<'a> TraceUri<'a> {
    /// Creates a redacted URI trace view with the path shown as-is.
    pub fn new(uri: &'a str) -> Self {
        Self(uri)
    }
}

impl fmt::Display for TraceUri<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let uri = self.0.split('#').next().unwrap_or_default();
        let (base, query) = uri
            .split_once('?')
            .map_or((uri, None), |(base, query)| (base, Some(query)));
        let authority_start = base
            .find("://")
            .map_or(if base.starts_with("//") { 2 } else { 0 }, |i| i + 3);
        let (prefix, rest) = base.split_at(authority_start);
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let authority = authority.rsplit('@').next().unwrap_or_default();

        write!(f, "{prefix}{authority}{path}")?;
        if let Some(query) = query {
            f.write_str("?")?;
            for (index, parameter) in query.split('&').filter(|p| !p.is_empty()).enumerate() {
                if index != 0 {
                    f.write_str("&")?;
                }
                if let Some((name, _)) = parameter.split_once('=') {
                    write!(f, "{name}={REDACTED}")?;
                } else {
                    f.write_str(REDACTED)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceHeaders, TraceUri};

    #[test]
    fn http_trace_headers_redact_all_unlisted_values() {
        let headers = [
            (
                "authorization",
                "AWS4-HMAC-SHA256 Credential=AKIDSECRET/...,Signature=deadbeefsig",
            ),
            ("x-amz-security-token", "SESSIONTOKENSECRET"),
            ("cookie", "c=COOKIESECRET"),
            ("proxy-authorization", "Basic PROXYSECRET"),
            ("x-github-token-ish", "ARBITRARYSECRET"),
            ("content-length", "0"),
            ("host", "bucket.s3.amazonaws.com"),
            ("x-amz-date", "20261101T120000Z"),
        ]
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()));

        let rendered = TraceHeaders::new(&headers).to_string();

        for secret in [
            "AKIDSECRET",
            "deadbeefsig",
            "SESSIONTOKENSECRET",
            "COOKIESECRET",
            "PROXYSECRET",
            "ARBITRARYSECRET",
        ] {
            assert!(
                !rendered.contains(secret),
                "trace leaked {secret}: {rendered}"
            );
        }
        assert_eq!(
            rendered,
            "authorization=<redacted>; x-amz-security-token=<redacted>; cookie=<redacted>; proxy-authorization=<redacted>; x-github-token-ish=<redacted>; content-length=0; host=bucket.s3.amazonaws.com; x-amz-date=20261101T120000Z"
        );
    }

    #[test]
    fn http_trace_headers_allowlist_is_case_insensitive() {
        let headers = [
            ("Host", "bucket.s3.amazonaws.com"),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("TRANSFER-ENCODING", "chunked"),
            ("Expect", "100-continue"),
            ("User-Agent", "carina"),
            ("X-Amz-Date", "20261101T120000Z"),
            ("AUTHORIZATION", "ARBITRARYSECRET"),
        ]
        .map(|(name, value)| (name.to_string(), value.as_bytes().to_vec()));

        assert_eq!(
            TraceHeaders::new(&headers).to_string(),
            "Host=bucket.s3.amazonaws.com; Content-Type=application/json; Content-Length=0; TRANSFER-ENCODING=chunked; Expect=100-continue; User-Agent=carina; X-Amz-Date=20261101T120000Z; AUTHORIZATION=<redacted>"
        );
    }

    #[test]
    fn http_trace_headers_redact_non_utf8_values() {
        let headers = [("x-unknown".to_string(), b"\xffARBITRARYSECRET".to_vec())];
        assert_eq!(
            TraceHeaders::new(&headers).to_string(),
            "x-unknown=<redacted>"
        );
        assert_eq!(TraceHeaders::new(&[]).to_string(), "");
    }

    #[test]
    fn http_trace_uri_redacts_userinfo_query_values_and_fragment() {
        let uri = "https://user:PASSSECRET@bucket.s3.amazonaws.com/key?X-Amz-Credential=AKIDSECRET%2F...&X-Amz-Signature=SIGSECRET&X-Amz-Security-Token=TOKSECRET#FRAGSECRET";

        let rendered = TraceUri::new(uri).to_string();

        for secret in [
            "user",
            "PASSSECRET",
            "AKIDSECRET",
            "SIGSECRET",
            "TOKSECRET",
            "FRAGSECRET",
        ] {
            assert!(
                !rendered.contains(secret),
                "trace leaked {secret}: {rendered}"
            );
        }
        assert_eq!(
            rendered,
            "https://bucket.s3.amazonaws.com/key?X-Amz-Credential=<redacted>&X-Amz-Signature=<redacted>&X-Amz-Security-Token=<redacted>"
        );
    }

    #[test]
    fn http_trace_uri_redacts_every_query_value_regardless_of_name() {
        let uri = "http://localhost:8080/key?prefix=ARBITRARYSECRET&flag&prefix=&=TOKSECRET&token=first=SIGSECRET";
        assert_eq!(
            TraceUri::new(uri).to_string(),
            "http://localhost:8080/key?prefix=<redacted>&<redacted>&prefix=<redacted>&=<redacted>&token=<redacted>"
        );
    }

    #[test]
    fn http_trace_uri_transforms_request_targets() {
        for (uri, expected) in [
            ("https://example.com/path", "https://example.com/path"),
            ("https://example.com#FRAGSECRET", "https://example.com"),
            ("#FRAGSECRET", ""),
            ("https://example.com/path?", "https://example.com/path?"),
            ("https://h/p?AKIDSECRET", "https://h/p?<redacted>"),
            (
                "/key?prefix=ARBITRARYSECRET#FRAGSECRET",
                "/key?prefix=<redacted>",
            ),
            (
                "//user:PASS@localhost:8080/key?prefix=X",
                "//localhost:8080/key?prefix=<redacted>",
            ),
            (
                "https://user:PASSSECRET@[2001:db8::1]:8443/a%2Fb?prefix=TOKSECRET",
                "https://[2001:db8::1]:8443/a%2Fb?prefix=<redacted>",
            ),
            ("bucket.s3.amazonaws.com:443", "bucket.s3.amazonaws.com:443"),
            ("*", "*"),
            (
                "user:PASSSECRET@bucket.s3.amazonaws.com/key",
                "bucket.s3.amazonaws.com/key",
            ),
            ("https://a:PASSSECRET@b@host/p", "https://host/p"),
        ] {
            let rendered = TraceUri::new(uri).to_string();
            assert_eq!(rendered, expected, "input: {uri}");
            for secret in ["PASSSECRET", "AKIDSECRET", "TOKSECRET", "FRAGSECRET"] {
                assert!(
                    !rendered.contains(secret),
                    "trace leaked {secret}: {rendered}"
                );
            }
        }
    }
}
