//! Body framing for wasi:http outgoing requests.
//!
//! # Two invariants enforced here
//!
//! 1. **Every buffered body crosses the wasi:http boundary with an explicit
//!    `Content-Length`** — see `RequestBody` / `inject_content_length_header`
//!    below (carina-rs/carina#3254).
//! 2. **Every call into `wasi:io::OutputStream::blocking_write_and_flush`
//!    carries at most `BLOCKING_WRITE_AND_FLUSH_MAX_BYTES` bytes** — see
//!    `chunks_for_blocking_write` below (carina-rs/carina#3318). The
//!    wasi:io contract caps a single `blocking-write-and-flush` at
//!    4096 bytes; the host `wasmtime-wasi-io` returns a *trap* (not a
//!    recoverable error) on overflow, which leaves the WASM instance
//!    re-entry-locked. A 4157-byte CloudControl `UpdateResource` body
//!    triggered this; the splitter restores the contract.
//!
//! # Why this exists
//!
//! `wasi:http` and the host-side `wasmtime_wasi_http` bridge let the guest
//! emit only two body shapes to hyper:
//!
//! - **Sized**: a `Content-Length` header is present in the request, so the
//!   host constructs a `HostOutgoingBody` with `size = Some(n)` and hyper
//!   serializes it with an explicit `Content-Length` framing.
//! - **Unknown-size**: no `Content-Length` is present, so the host
//!   constructs a `HostOutgoingBody` with `size = None`, and hyper's
//!   HTTP/1.1 client falls back to `Transfer-Encoding: chunked`.
//!
//! The AWS SDK Rust does **not** consistently emit `Content-Length: 0` for
//! body-less DELETE operations. When such a request crosses the
//! wasi:http boundary, the host has no length signal and hyper sends the
//! DELETE with `Transfer-Encoding: chunked`. S3 rejects body-less DELETE
//! framed this way — it sits on the socket until the 20s server-side
//! idle cutoff fires (`HTTP 400 RequestTimeout`). Every body-less DELETE
//! the WASM provider makes pays the same 20s penalty.
//!
//! That's the chain pinned by the diagnosis in carina-rs/carina#3254
//! (PRs #3257 / #3261 / #3264 / #3268 all merged). The bytes never reach
//! S3 on time because hyper waits for a chunked terminator that the
//! body never produces.
//!
//! # The fix
//!
//! Restore the invariant at the wasi:http boundary: **every outgoing
//! request body that the SDK has fully buffered (which is every body the
//! current `WasiHttpConnector` ever sees) crosses the boundary with an
//! explicit `Content-Length`**. The header is added when missing and
//! left alone when already present.
//!
//! The classification is encoded as a tagged union so the broken state
//! ("body bytes known but Content-Length missing") is unrepresentable
//! after `RequestBody::from_sdk_body` runs: every constructor of
//! `RequestBody` produces a value that, when handed to
//! `inject_content_length_header`, emits a sized framing on the wire.

/// The body of an outgoing request, classified by what we know about its
/// size at the wasi:http boundary.
///
/// The AWS SDK Rust's `HttpRequest` body is always fully buffered by the
/// time `WasiHttpConnector` sees it (the smithy runtime collects it
/// before calling the connector), so only the `Empty` and `Sized`
/// variants are reachable from the current connector. A `Streaming`
/// variant would be added if/when the connector grows to handle
/// chunked-upload paths — encoded explicitly so the wasi:http bridge
/// knows when *not* to inject `Content-Length`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestBody {
    /// No request body. The SDK either passed no bytes or passed a
    /// zero-length buffer. Either way the wire framing must be
    /// `Content-Length: 0` so hyper does not fall back to
    /// `Transfer-Encoding: chunked`.
    Empty,
    /// A fully-buffered, non-empty body. The wire framing must carry
    /// `Content-Length: <bytes.len()>`.
    Sized(Vec<u8>),
}

impl RequestBody {
    /// Classify a buffered SDK body into `Empty` / `Sized`.
    ///
    /// The input is the byte slice the AWS SDK handed down through
    /// `request.body().bytes().unwrap_or(&[])`. Empty buffers — the
    /// case that triggers carina-rs/carina#3254 — collapse to
    /// `RequestBody::Empty`.
    pub(crate) fn from_sdk_body(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            RequestBody::Empty
        } else {
            RequestBody::Sized(bytes.to_vec())
        }
    }

    /// The number of bytes the receiver should expect on the wire.
    pub(crate) fn content_length(&self) -> usize {
        match self {
            RequestBody::Empty => 0,
            RequestBody::Sized(bytes) => bytes.len(),
        }
    }

    /// The bytes to feed into the wasi:http `OutgoingBody` output
    /// stream. Returns an empty slice for `Empty`.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            RequestBody::Empty => &[],
            RequestBody::Sized(bytes) => bytes,
        }
    }
}

/// Maximum bytes accepted by a single
/// `wasi:io::OutputStream::blocking_write_and_flush` call.
///
/// Pinned by the wasi:io WIT contract; the host implementation
/// (`wasmtime-wasi-io::impls::OutputStream::blocking_write_and_flush`)
/// returns a `StreamError::trap` — not a recoverable error — when the
/// guest passes more than this. A trap on the wasi:io path poisons the
/// component instance from wasmtime's point of view, so the very next
/// guest entry fails with `cannot enter component instance` (the
/// cascade observed in carina-rs/carina#3318). Always feed bodies
/// through [`chunks_for_blocking_write`] before handing them to the
/// stream.
pub(crate) const BLOCKING_WRITE_AND_FLUSH_MAX_BYTES: usize = 4096;

/// Split `data` into chunks no larger than
/// [`BLOCKING_WRITE_AND_FLUSH_MAX_BYTES`], in order, so each chunk can
/// be passed to `wasi:io::OutputStream::blocking_write_and_flush`
/// without tripping the host-side trap.
///
/// Empty input yields no chunks (the caller must skip the stream write
/// entirely, the same as before the splitter existed). Concatenating
/// the returned chunks in order reproduces the input exactly.
pub(crate) fn chunks_for_blocking_write(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    data.chunks(BLOCKING_WRITE_AND_FLUSH_MAX_BYTES)
}

/// Add a `content-length` header to `headers` if not already present.
///
/// Headers come in as the `(name, value-bytes)` list that
/// `wasi::http::types::Fields::from_list` accepts. Matching is
/// case-insensitive on the header name to handle the AWS SDK's
/// historical mix of `Content-Length` / `content-length` spellings.
///
/// If a `content-length` header is already present we leave it alone —
/// either the SDK already framed the request correctly, or there is a
/// genuine mismatch and we want it to surface (e.g. as an HTTP error)
/// rather than silently overwriting.
pub(crate) fn inject_content_length_header(
    headers: &mut Vec<(String, Vec<u8>)>,
    body: &RequestBody,
) {
    let already_present = headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    if already_present {
        return;
    }
    headers.push((
        "content-length".to_string(),
        body.content_length().to_string().into_bytes(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sdk_body_empty_slice_is_empty() {
        assert_eq!(RequestBody::from_sdk_body(&[]), RequestBody::Empty);
    }

    #[test]
    fn from_sdk_body_non_empty_is_sized() {
        let bytes = b"hello".to_vec();
        assert_eq!(
            RequestBody::from_sdk_body(b"hello"),
            RequestBody::Sized(bytes)
        );
    }

    #[test]
    fn content_length_matches_byte_count() {
        assert_eq!(RequestBody::Empty.content_length(), 0);
        assert_eq!(RequestBody::Sized(vec![0; 7]).content_length(), 7);
    }

    #[test]
    fn empty_body_injects_content_length_zero() {
        let mut headers: Vec<(String, Vec<u8>)> = vec![
            ("host".to_string(), b"example.com".to_vec()),
            ("x-amz-date".to_string(), b"20260526T000000Z".to_vec()),
        ];
        inject_content_length_header(&mut headers, &RequestBody::Empty);
        let cl = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .expect("content-length must be present after injection");
        assert_eq!(cl.1, b"0".to_vec());
    }

    #[test]
    fn sized_body_injects_content_length_matching_bytes() {
        let mut headers: Vec<(String, Vec<u8>)> = vec![];
        inject_content_length_header(&mut headers, &RequestBody::Sized(b"hello world".to_vec()));
        let cl = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .expect("content-length must be present after injection");
        assert_eq!(cl.1, b"11".to_vec());
    }

    #[test]
    fn existing_content_length_is_left_untouched() {
        // SDK already framed the body. Don't second-guess it.
        let mut headers: Vec<(String, Vec<u8>)> =
            vec![("Content-Length".to_string(), b"42".to_vec())];
        inject_content_length_header(&mut headers, &RequestBody::Empty);
        // Still exactly one entry, original value preserved.
        let entries: Vec<_> = headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1, b"42".to_vec());
    }

    #[test]
    fn chunks_empty_yields_nothing() {
        let chunks: Vec<&[u8]> = chunks_for_blocking_write(&[]).collect();
        assert!(chunks.is_empty());
    }

    #[test]
    fn chunks_under_limit_yields_single_chunk() {
        let data = vec![0xAB; BLOCKING_WRITE_AND_FLUSH_MAX_BYTES - 1];
        let chunks: Vec<&[u8]> = chunks_for_blocking_write(&data).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), BLOCKING_WRITE_AND_FLUSH_MAX_BYTES - 1);
    }

    #[test]
    fn chunks_exactly_at_limit_yields_single_chunk() {
        let data = vec![0xCD; BLOCKING_WRITE_AND_FLUSH_MAX_BYTES];
        let chunks: Vec<&[u8]> = chunks_for_blocking_write(&data).collect();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
    }

    #[test]
    fn chunks_over_limit_yields_multiple_bounded_chunks() {
        // 4157-byte body — the size of the CloudControl `UpdateResource`
        // call that triggered carina-rs/carina#3318 on the real
        // `awscc.iam.RolePolicy` update.
        let data = vec![0xEF; 4157];
        let chunks: Vec<&[u8]> = chunks_for_blocking_write(&data).collect();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
        assert_eq!(chunks[1].len(), 4157 - BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
        // Reassembled, the chunks must equal the input.
        let mut joined: Vec<u8> = Vec::new();
        for c in &chunks {
            joined.extend_from_slice(c);
        }
        assert_eq!(joined, data);
        // Every chunk is within the wasi:io limit.
        for c in &chunks {
            assert!(c.len() <= BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
        }
    }

    #[test]
    fn chunks_far_over_limit_yields_only_bounded_chunks() {
        // Stress: ten-times the limit plus a tail.
        let total = BLOCKING_WRITE_AND_FLUSH_MAX_BYTES * 10 + 123;
        let data = vec![0x42u8; total];
        let chunks: Vec<&[u8]> = chunks_for_blocking_write(&data).collect();
        let mut sum = 0;
        for c in &chunks {
            assert!(c.len() <= BLOCKING_WRITE_AND_FLUSH_MAX_BYTES);
            sum += c.len();
        }
        assert_eq!(sum, total);
        assert_eq!(chunks.len(), 11);
    }

    #[test]
    fn case_insensitive_existing_content_length_is_detected() {
        // The AWS SDK has historically emitted both `Content-Length`
        // and `content-length`; either spelling must inhibit injection.
        for existing_name in ["content-length", "Content-Length", "CONTENT-LENGTH"] {
            let mut headers: Vec<(String, Vec<u8>)> =
                vec![(existing_name.to_string(), b"5".to_vec())];
            inject_content_length_header(&mut headers, &RequestBody::Sized(b"world".to_vec()));
            let entries: Vec<_> = headers
                .iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                .collect();
            assert_eq!(
                entries.len(),
                1,
                "duplicate content-length when existing spelling was {existing_name}",
            );
            assert_eq!(entries[0].1, b"5".to_vec());
        }
    }
}
