//! Host-owned stderr for provider guests. Retain a bounded sanitized prefix
//! and count the entire discarded tail before emitting one line at its end.

use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use carina_core::provider::ProviderValidationInputs;
use carina_core::provider_text::GuestInputInventory;
use carina_core::provider_text::RedactionSet;
use carina_core::provider_text::{MAX_PROVIDER_TEXT_BYTES, RedactingLineWriter};
use hyper::body::Bytes;
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError, StreamResult};

/// All handles for one guest stderr share a buffer and a host-owned sink.
#[derive(Clone)]
pub(in crate::wasm_factory) struct GuestStderr {
    state: Arc<Mutex<LineBuffer>>,
}

struct LineBuffer {
    writer: Option<RedactingLineWriter<SharedSink>>,
    session: Option<RedactionSet>,
    utf8_tail: Vec<u8>,
    sink: SharedSink,
    failed: bool,
}
#[derive(Clone)]
struct SharedSink(Arc<Mutex<Box<dyn Write + Send>>>);
impl Write for SharedSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush()
    }
}
impl GuestStderr {
    pub(in crate::wasm_factory) fn new(sink: impl Write + Send + 'static) -> Self {
        let sink = SharedSink(Arc::new(Mutex::new(Box::new(sink))));
        Self {
            state: Arc::new(Mutex::new(LineBuffer {
                writer: Some(RedactingLineWriter::for_metadata(sink.clone())),
                session: None,
                utf8_tail: Vec::with_capacity(char::MAX.len_utf8()),
                sink,
                failed: false,
            })),
        }
    }
    #[cfg(test)]
    pub(in crate::wasm_factory) fn begin_call(
        &self,
        session: &ProviderValidationInputs,
    ) -> GuestStderrCall {
        self.begin_inventory_call(session, &session.guest_inventory())
    }
    pub(in crate::wasm_factory) fn begin_inventory_call(
        &self,
        session: &ProviderValidationInputs,
        inputs: &GuestInputInventory,
    ) -> GuestStderrCall {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = state.flush_pending();
        state.session = Some(session.redaction_set().clone());
        state.writer = Some(inputs.line_writer(state.sink.clone()));
        GuestStderrCall(self.clone())
    }
    pub(in crate::wasm_factory) fn known_secrets(&self) -> Option<RedactionSet> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session
            .clone()
    }

    pub(in crate::wasm_factory) fn write_line(&self, text: &str) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.write_bytes(text.as_bytes())?;
        state.write_bytes(b"\n")?;
        state.flush_pending()
    }

    fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_bytes(bytes)
    }

    fn flush_pending(&self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_pending()
    }

    fn flush_sink(&self) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .flush_sink()
    }
}

pub(in crate::wasm_factory) struct GuestStderrCall(GuestStderr);
impl Drop for GuestStderrCall {
    fn drop(&mut self) {
        let _ = self.0.flush_pending();
    }
}
impl Drop for LineBuffer {
    fn drop(&mut self) {
        let _ = self.flush_pending();
    }
}
impl LineBuffer {
    fn push_str(&mut self, text: &str) -> io::Result<()> {
        let result = self
            .writer
            .as_mut()
            .expect("stderr writer installed")
            .push_str(text);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn ensure_open(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "guest stderr sink is closed",
            ))
        } else {
            Ok(())
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.ensure_open()?;
        if self.utf8_tail.is_empty() {
            self.decode_utf8_chunk(bytes)
        } else {
            let mut pending = std::mem::take(&mut self.utf8_tail);
            pending.extend_from_slice(bytes);
            self.decode_utf8_chunk(&pending)
        }
    }

    fn decode_utf8_chunk(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        loop {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    if !text.is_empty() {
                        self.push_str(text)?;
                    }
                    return Ok(());
                }
                Err(error) => {
                    let valid = std::str::from_utf8(&bytes[..error.valid_up_to()])
                        .expect("UTF-8 error identifies a valid prefix");
                    if !valid.is_empty() {
                        self.push_str(valid)?;
                    }
                    bytes = &bytes[error.valid_up_to()..];
                    match error.error_len() {
                        Some(length) => {
                            self.push_str("\u{fffd}")?;
                            bytes = &bytes[length..];
                        }
                        // At most three bytes can await a scalar's completion.
                        None => {
                            self.utf8_tail.extend_from_slice(bytes);
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        if !self.utf8_tail.is_empty() {
            self.push_str("\u{fffd}")?;
            self.utf8_tail.clear();
        }
        self.writer
            .as_mut()
            .expect("stderr writer installed")
            .finish()?;
        self.flush_sink()
    }

    fn flush_sink(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        let result = self.sink.flush();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
}

impl IsTerminal for GuestStderr {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for GuestStderr {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for GuestStderr {
    async fn ready(&mut self) {}
}

impl OutputStream for GuestStderr {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.write_bytes(&bytes)
            .map_err(|error| StreamError::LastOperationFailed(wasmtime::Error::new(error)))
    }

    fn flush(&mut self) -> StreamResult<()> {
        self.flush_sink()
            .map_err(|error| StreamError::LastOperationFailed(wasmtime::Error::new(error)))
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failed
        {
            Err(StreamError::Closed)
        } else {
            Ok(MAX_PROVIDER_TEXT_BYTES)
        }
    }
}

impl AsyncWrite for GuestStderr {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.write_bytes(bytes).map(|()| bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.flush_sink())
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(self.flush_pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_text_fixtures::*;
    use tokio::io::AsyncWriteExt;

    mod round5 {
        include!("guest_stderr/tests/round5.rs");
    }

    struct StderrConfigFactory;
    impl carina_core::provider::ProviderFactory for StderrConfigFactory {
        fn name(&self) -> &str {
            "stderr-test"
        }
        fn display_name(&self) -> &str {
            "stderr test"
        }
        fn validate_config(
            &self,
            _: &carina_core::provider::CanonicalProviderConfigAttributes,
            _session: &carina_core::provider::ProviderValidationInputs,
        ) -> carina_core::provider::ProviderResult<()> {
            Ok(())
        }
        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            carina_core::schema::ValidatedConfigSchema::validate(std::collections::BTreeMap::new())
                .unwrap()
        }
        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            Vec::new()
        }
        fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
            carina_core::schema::ValidatedProviderSchemaWitness::new(
                Vec::new(),
                self.provider_config_attribute_types(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }
        fn create_provider(
            &self,
            _: &carina_core::provider::ProviderSession,
            _: carina_core::provider::InitializeOutcomeMint,
        ) -> carina_core::provider::BoxFuture<
            '_,
            carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
        > {
            unreachable!("stderr test only prepares config")
        }
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn write_all(stream: &mut dyn OutputStream, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let size = stream.check_write().unwrap().min(bytes.len());
            assert!(size > 0);
            stream
                .write(Bytes::copy_from_slice(&bytes[..size]))
                .unwrap();
            bytes = &bytes[size..];
        }
    }

    fn call_secrets(plaintext: &str) -> ProviderValidationInputs {
        crate::provider_text_fixtures::context(&[plaintext])
            .session()
            .guest_inputs()
    }

    #[test]
    fn review_round7_guest_stderr_large_chunks_avoid_per_byte_refresh_and_locks() {
        const CHUNK: usize = 4093;
        const SPLIT: &str = "split-private-4305";
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let context = crate::provider_text_fixtures::context(&[
            "volume-private-4305",
            "unicode-秘密-4305",
            SPLIT,
        ]);
        let call = stderr.begin_call(&context.session().guest_inputs());
        let line = "2026-11-07T12:34:56Z DEBUG provider::request public-context token=volume-private-4305 unicode-秘密-4305\n";
        let mut volume = line.repeat((1024usize * 1024).div_ceil(line.len()));
        volume.push_str(&"p".repeat((CHUNK - volume.len() % CHUNK + CHUNK - 8) % CHUNK));
        volume.push_str(SPLIT);
        volume.push_str(" public-tail\n");
        let writes = volume.as_bytes().chunks(CHUNK).count();
        for chunk in volume.as_bytes().chunks(CHUNK) {
            stderr.write_bytes(chunk).unwrap();
        }
        drop(call);
        let output = capture.text();
        for secret in ["volume-private-4305", "unicode-秘密-4305", SPLIT] {
            assert!(!output.contains(secret), "large stderr leaked {secret}");
        }
        assert!(output.contains("public-context"));
        assert!(output.contains("(secret) public-tail"));
        let (refresh_checks, snapshot_locks) = stderr
            .state
            .lock()
            .unwrap()
            .writer
            .as_ref()
            .unwrap()
            .redaction_stats_for_test();
        eprintln!(
            "stderr redaction counters: {writes} writes, {refresh_checks} refresh checks, {snapshot_locks} snapshot locks"
        );
        assert!(
            refresh_checks <= writes + 2,
            "{refresh_checks} refresh checks for {writes} decoded writes"
        );
        assert!(
            snapshot_locks <= 2,
            "unchanged inventory acquired {snapshot_locks} snapshot locks"
        );
    }

    #[test]
    fn guest_stderr_uses_current_call_then_instance_union_without_a_call() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        {
            let _call = stderr.begin_call(
                &crate::provider_text_fixtures::context(&["first-private", "second-private"])
                    .session()
                    .guest_inputs(),
            );
            write_all(stream.as_mut(), b"first-private\n");
        }
        {
            let _call = stderr.begin_call(
                &crate::provider_text_fixtures::context(&["first-private", "second-private"])
                    .session()
                    .guest_inputs(),
            );
            write_all(stream.as_mut(), b"first-private second-private\n");
        }
        write_all(stream.as_mut(), b"first-private second-private\n");
        assert_eq!(
            capture.text(),
            "(secret)\n(secret) (secret)\n(secret) (secret)\n"
        );
        let independent = Capture::default();
        let other = GuestStderr::new(independent.clone());
        write_all(other.p2_stream().as_mut(), b"first-private\n");
        assert_eq!(independent.text(), "first-private\n");
    }

    #[test]
    fn review_a1_poison_relay_does_not_rescan_inserted_masks() {
        let context = call_secrets("secret");
        let stderr = GuestStderr::new(Capture::default());
        drop(stderr.begin_call(&context));
        let reason = super::super::PoisonReason::from_trap(
            "create",
            &wasmtime::Error::msg("secret"),
            stderr.known_secrets().as_ref(),
        );
        let error = reason
            .fail_fast_error("delete")
            .with_secrets_for_test(&crate::provider_text_fixtures::context(&["secret"]));
        assert_eq!(
            error.to_string(),
            "WASM provider instance unusable after trap in create: (secret) (operation 'delete'); re-run the command"
        );
    }

    #[test]
    fn review_a2_metadata_relay_retains_prior_call_context() {
        let context =
            crate::provider_text_fixtures::context(&["prior-private", "different-private"]);
        let cause =
            crate::wasm_factory::MetadataDiagnostic::new(std::io::Error::other("prior-private"));
        let relayed =
            carina_core::provider::ProviderError::internal_caused_by("public context", cause);
        let received = relayed.with_secrets_for_test(&context);
        assert_eq!(received.to_string(), "public context: (secret)");
    }

    #[test]
    fn review_a4_guest_stderr_in_flight_retains_previous_resource_secrets() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        drop(
            stderr.begin_call(
                &crate::provider_text_fixtures::context(&[
                    "resource-a-password",
                    "resource-b-password",
                ])
                .session()
                .guest_inputs(),
            ),
        );
        let call = stderr.begin_call(
            &crate::provider_text_fixtures::context(&[
                "resource-a-password",
                "resource-b-password",
            ])
            .session()
            .guest_inputs(),
        );
        write_all(
            stream.as_mut(),
            b"resource-a-password resource-b-password\n",
        );
        drop(call);
        assert_eq!(capture.text(), "(secret) (secret)\n");
    }

    #[test]
    fn guest_stderr_known_secrets_protect_poisoned_instance_diagnostics() {
        let stderr = GuestStderr::new(Capture::default());
        let call = stderr.begin_call(&call_secrets("previous-call-private"));
        drop(call);
        let reason = super::super::PoisonReason::from_trap(
            "create",
            &wasmtime::Error::msg("previous-call-private"),
            stderr.known_secrets().as_ref(),
        );
        assert!(!format!("{reason:?}").contains("previous-call-private"));
        let error = reason.fail_fast_error("delete").with_secrets_for_test(
            &crate::provider_text_fixtures::context(&["previous-call-private"]),
        );
        assert!(error.to_string().contains("(secret)"));
        assert!(!error.to_string().contains("previous-call-private"));
    }

    #[test]
    fn guest_stderr_masks_multiline_secrets_before_line_framing() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        let call = stderr.begin_call(&call_secrets("private\nmultiline"));
        write_all(stream.as_mut(), b"public private\n");
        assert!(!capture.text().contains("private"));
        write_all(stream.as_mut(), b"multiline context\n");
        drop(call);
        assert_eq!(capture.text(), "public (secret) context\n");
        write_all(stream.as_mut(), b"teardown private\nmultiline\n");
        drop(stream);
        drop(stderr);
        assert_eq!(
            capture.text(),
            "public (secret) context\nteardown (secret)\n"
        );
    }

    #[test]
    fn guest_stderr_call_drop_flushes_partial_secret_and_protects_teardown() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        let call = stderr.begin_call(&call_secrets("private-tail"));
        write_all(stream.as_mut(), b"private-");
        write_all(stream.as_mut(), b"tail");
        assert!(capture.text().is_empty());
        drop(call);
        assert_eq!(capture.text(), "(secret)\n");
        write_all(stream.as_mut(), b"teardown private-tail");
        drop(stream);
        drop(stderr);
        assert_eq!(capture.text(), "(secret)\nteardown (secret)\n");
    }

    /// Unit-level coverage of the actual forwarding sink, without a compiled
    /// WASM fixture. Split writes also exercise shared line buffering.
    #[test]
    fn guest_stderr_secret_plaintext_is_redacted_before_forwarding() {
        const SECRET: &str = "guest-stderr-private-4305";
        const MASK: &str = "(secret)";
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let values = indexmap::IndexMap::from([(
            "password".to_owned(),
            carina_core::resource::Value::Deferred(carina_core::resource::DeferredValue::Secret(
                Box::new(carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String(SECRET.into()),
                ))
                .into(),
            )),
        )]);
        let factory = StderrConfigFactory;
        let _config =
            carina_core::executor::provider_ready::validate_and_canonicalize_provider_config(
                &factory,
                "mock",
                &values,
                &carina_core::provider::ProviderFactory::provider_config_attribute_types(&factory),
            )
            .unwrap();
        let call = stderr.begin_call(&call_secrets(SECRET));
        let mut stream = stderr.p2_stream();
        write_all(
            stream.as_mut(),
            b"provider: public-context-4305 password=guest-stderr-",
        );
        write_all(stream.as_mut(), b"private-4305\n");
        drop(stream);
        drop(call);
        drop(stderr);
        let text = capture.text();
        assert!(
            text.contains("public-context-4305"),
            "non-secret stderr context was lost: {text}"
        );
        assert!(
            !text.contains(SECRET),
            "provider secret plaintext leaked through guest stderr forwarding: {text}"
        );
        assert!(
            text.contains(MASK),
            "missing {MASK} in forwarded guest stderr: {text}"
        );
    }

    #[test]
    fn guest_stderr_omits_the_entire_oversized_tail_until_newline_or_final_drop() {
        use carina_core::provider_text::DiagnosticText as ProviderText;
        for terminate_with_newline in [false, true] {
            let capture = Capture::default();
            let stderr = GuestStderr::new(capture.clone());
            let mut stream = stderr.p2_stream();
            let raw = format!(
                "\x1b[2J{}\x1b]0;tail\x07",
                "x".repeat(10 * MAX_PROVIDER_TEXT_BYTES)
            );
            write_all(stream.as_mut(), raw.as_bytes());
            stream.flush().unwrap();
            assert!(
                capture.text().is_empty(),
                "an oversized line must wait for its final omitted-byte count"
            );
            if terminate_with_newline {
                write_all(stream.as_mut(), b"\n");
            }
            drop(stream);
            drop(stderr);
            let output = capture.text();
            assert_eq!(output.lines().count(), 1);
            assert_eq!(output, format!("{}\n", ProviderText::new(raw)));
            assert!(!output.contains(['\x1b', '\x07']));
            assert!(output.trim_end_matches('\n').len() <= MAX_PROVIDER_TEXT_BYTES);
        }
    }

    #[test]
    fn guest_stderr_streaming_lossy_utf8_matches_whole_line_decoding() {
        use carina_core::provider_text::DiagnosticText as ProviderText;
        for oversized in [false, true] {
            let capture = Capture::default();
            let stderr = GuestStderr::new(capture.clone());
            let mut stream = stderr.p2_stream();
            let mut raw = if oversized {
                vec![b'x'; MAX_PROVIDER_TEXT_BYTES - 1]
            } else {
                Vec::new()
            };
            raw.extend_from_slice("🦀".as_bytes());
            raw.extend_from_slice(b"\xff\xf0\x9f[\x1b]\xc2\x9b");
            if oversized {
                raw.extend(vec![b'y'; 3 * MAX_PROVIDER_TEXT_BYTES]);
            }
            raw.extend_from_slice(b"\xe2\x82");
            for byte in &raw {
                write_all(stream.as_mut(), &[*byte]);
                assert!(stderr.state.lock().unwrap().utf8_tail.len() < char::MAX.len_utf8());
            }
            drop(stream);
            drop(stderr);
            assert_eq!(
                capture.text(),
                format!("{}\n", ProviderText::new(String::from_utf8_lossy(&raw)))
            );
        }
    }

    #[test]
    fn guest_stderr_streams_sanitized_lines_and_caps_long_lines() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(stream.as_mut(), b"\x1b[2J");
        assert!(
            capture.text().is_empty(),
            "partial lines should remain buffered"
        );
        write_all(stream.as_mut(), b"\x1b]0;evil\x07\n");
        assert_eq!(capture.text(), "\\u{1b}[2J\\u{1b}]0;evil\\u{7}\n");

        write_all(
            stream.as_mut(),
            &vec![b'x'; 3 * MAX_PROVIDER_TEXT_BYTES + 17],
        );
        assert_eq!(
            capture.text().lines().count(),
            1,
            "the oversized line is still pending"
        );
        assert!(stderr.state.lock().unwrap().utf8_tail.len() < char::MAX.len_utf8());
        write_all(stream.as_mut(), b"\n");
        let text = capture.text();
        assert!(!text.as_bytes().contains(&0x1b));
        assert!(!text.as_bytes().contains(&0x07));
        assert_eq!(text.lines().count(), 2);
        assert!(
            text.lines()
                .all(|line| line.len() <= MAX_PROVIDER_TEXT_BYTES)
        );
        for line in text.lines().filter(|line| line.contains("… (truncated, ")) {
            let (prefix, marker) = line.split_once("… (truncated, ").unwrap();
            let omitted: usize = marker.strip_suffix(" bytes)").unwrap().parse().unwrap();
            assert_eq!(prefix.len() + omitted, 3 * MAX_PROVIDER_TEXT_BYTES + 17);
        }
    }

    #[tokio::test]
    async fn guest_stderr_keeps_one_line_across_flushed_dropped_handles() {
        for use_async in [false, true] {
            let capture = Capture::default();
            let stderr = GuestStderr::new(capture.clone());
            for piece in [
                b"a ".as_slice(),
                b"\x1b",
                b"[2J",
                b"\xf0\x9f",
                b"\xa6\x80",
                b" b",
            ] {
                if use_async {
                    let mut handle = Pin::from(stderr.async_stream());
                    handle.write_all(piece).await.unwrap();
                    handle.flush().await.unwrap();
                    drop(handle);
                } else {
                    let mut handle = stderr.p2_stream();
                    handle
                        .blocking_write_and_flush(Bytes::copy_from_slice(piece))
                        .await
                        .unwrap();
                    drop(handle);
                }
                assert!(
                    capture.text().is_empty(),
                    "flush or handle drop terminated a partial line"
                );
            }
            let mut handle = stderr.p2_stream();
            handle
                .blocking_write_and_flush(Bytes::from_static(b"\n"))
                .await
                .unwrap();
            drop(handle);
            drop(stderr);
            assert_eq!(capture.text(), "a \\u{1b}[2J🦀 b\n");
        }
    }

    #[test]
    fn guest_stderr_flushes_partial_line_once_when_shared_buffer_is_dropped() {
        use wasmtime::component::ResourceTable;
        use wasmtime_wasi::cli::WasiCliCtxView;
        use wasmtime_wasi::p2::bindings::cli::stderr::Host as _;

        let capture = Capture::default();
        let mut ctx = wasmtime_wasi::WasiCtxBuilder::new()
            .stderr(GuestStderr::new(capture.clone()))
            .build();
        let mut table = ResourceTable::new();
        let stream = WasiCliCtxView {
            ctx: ctx.cli(),
            table: &mut table,
        }
        .get_stderr()
        .unwrap();
        write_all(table.get_mut(&stream).unwrap().as_mut(), b"tail\x1b[2J");
        assert!(capture.text().is_empty());
        drop(table.delete(stream).unwrap());
        assert!(capture.text().is_empty());

        let stream = WasiCliCtxView {
            ctx: ctx.cli(),
            table: &mut table,
        }
        .get_stderr()
        .unwrap();
        write_all(table.get_mut(&stream).unwrap().as_mut(), b"teardown\x07");
        drop(ctx);
        assert!(capture.text().is_empty());
        drop(table);
        assert_eq!(capture.text(), "tail\\u{1b}[2Jteardown\\u{7}\n");
    }

    #[tokio::test]
    async fn guest_stderr_shares_utf8_buffer_between_p2_and_async_handles() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut p2 = stderr.p2_stream();
        let mut async_stream = Pin::from(stderr.async_stream());
        write_all(p2.as_mut(), b"a\xff\xf0\x9f");
        assert!(capture.text().is_empty());
        async_stream.write_all(b"\xa6\x80\x1b\\\n").await.unwrap();
        assert_eq!(capture.text(), "a\u{fffd}🦀\\u{1b}\\\n");
        async_stream.write_all(b"partial\x07").await.unwrap();
        async_stream.flush().await.unwrap();
        assert_eq!(capture.text(), "a\u{fffd}🦀\\u{1b}\\\n");
        async_stream.shutdown().await.unwrap();
        assert_eq!(capture.text(), "a\u{fffd}🦀\\u{1b}\\\npartial\\u{7}\n");
    }

    #[test]
    fn guest_stderr_keeps_utf8_at_truncation_boundaries_and_bounds_invalid_input() {
        use carina_core::provider_text::DiagnosticText as ProviderText;
        for suffix in ["あ".as_bytes(), "🦀".as_bytes()] {
            let capture = Capture::default();
            let stderr = GuestStderr::new(capture.clone());
            let mut stream = stderr.p2_stream();
            write_all(stream.as_mut(), &vec![b'x'; MAX_PROVIDER_TEXT_BYTES - 1]);
            for byte in suffix {
                write_all(stream.as_mut(), &[*byte]);
                assert!(stderr.state.lock().unwrap().utf8_tail.len() < char::MAX.len_utf8());
            }
            assert!(capture.text().is_empty());
            write_all(stream.as_mut(), b"\n");
            assert!(capture.text().contains("truncated,"));
            assert!(!capture.text().contains('\u{fffd}'));
        }

        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(stream.as_mut(), &vec![0xe2; 3 * MAX_PROVIDER_TEXT_BYTES]);
        assert!(stderr.state.lock().unwrap().utf8_tail.len() < char::MAX.len_utf8());
        assert!(capture.text().is_empty());
        write_all(stream.as_mut(), b"\n");
        assert_eq!(capture.text().lines().count(), 1);
        assert!(capture.text().contains('\u{fffd}'));
        assert_eq!(
            capture.text(),
            format!(
                "{}\n",
                ProviderText::new(String::from_utf8_lossy(&vec![
                    0xe2;
                    3 * MAX_PROVIDER_TEXT_BYTES
                ]))
            )
        );
        assert!(
            capture
                .text()
                .lines()
                .all(|line| line.len() <= MAX_PROVIDER_TEXT_BYTES)
        );
    }

    #[test]
    fn guest_stderr_preserves_delimiters_after_truncation_and_escapes_other_controls() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(stream.as_mut(), &vec![b'x'; MAX_PROVIDER_TEXT_BYTES + 1]);
        assert!(capture.text().is_empty());
        write_all(stream.as_mut(), b"\n\n");
        assert_eq!(capture.text().lines().count(), 2);
        assert_eq!(capture.text().lines().last(), Some(""));
        write_all(stream.as_mut(), "\r\x7f\u{9b}\n".as_bytes());
        assert_eq!(capture.text().lines().last(), Some("\\u{d}\\u{7f}\\u{9b}"));
    }

    #[test]
    fn guest_stderr_reports_sink_failures_and_does_not_replay_on_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct FailingSink(Arc<AtomicUsize>);
        impl Write for FailingSink {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let attempts = Arc::new(AtomicUsize::new(0));
        let stderr = GuestStderr::new(FailingSink(Arc::clone(&attempts)));
        let mut stream = stderr.p2_stream();
        let error = stream.write(Bytes::from_static(b"failed\n")).unwrap_err();
        let StreamError::LastOperationFailed(error) = error else {
            panic!("expected a typed sink error");
        };
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert!(matches!(stream.check_write(), Err(StreamError::Closed)));
        drop(stream);
        drop(stderr);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
