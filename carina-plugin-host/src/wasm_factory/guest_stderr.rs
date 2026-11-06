//! Host-owned stderr for provider guests. Retain a bounded sanitized prefix
//! and count the entire discarded tail before emitting one line at its end.

use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use carina_core::provider_text::{MAX_PROVIDER_TEXT_BYTES, ProviderTextBuilder};
use hyper::body::Bytes;
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError, StreamResult};

/// All handles for one guest stderr share a buffer and a host-owned sink.
#[derive(Clone)]
pub(super) struct GuestStderr {
    state: Arc<Mutex<LineBuffer>>,
}

struct LineBuffer {
    text: ProviderTextBuilder,
    utf8_tail: Vec<u8>,
    has_pending: bool,
    sink: Box<dyn Write + Send>,
    failed: bool,
}

impl GuestStderr {
    pub(super) fn new(sink: impl Write + Send + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(LineBuffer {
                text: ProviderTextBuilder::default(),
                utf8_tail: Vec::with_capacity(char::MAX.len_utf8()),
                has_pending: false,
                sink: Box::new(sink),
                failed: false,
            })),
        }
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

impl Drop for LineBuffer {
    fn drop(&mut self) {
        // Rust's WASI stdio repeatedly opens, flushes, and drops handles for
        // pieces of a single line. Only the final shared owner ends a partial
        // line, after both the context and its handles have been dropped.
        let _ = self.flush_pending();
    }
}

impl LineBuffer {
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
        for &byte in bytes {
            if byte == b'\n' {
                self.emit_line()?;
            } else {
                self.has_pending = true;
                self.utf8_tail.push(byte);
                self.decode_utf8_tail();
            }
        }
        Ok(())
    }

    fn decode_utf8_tail(&mut self) {
        loop {
            match std::str::from_utf8(&self.utf8_tail) {
                Ok(text) => {
                    self.text.push_str(text);
                    self.utf8_tail.clear();
                    return;
                }
                Err(error) => {
                    let valid = std::str::from_utf8(&self.utf8_tail[..error.valid_up_to()])
                        .expect("UTF-8 error identifies a valid prefix");
                    self.text.push_str(valid);
                    self.utf8_tail.drain(..error.valid_up_to());
                    match error.error_len() {
                        Some(length) => {
                            self.text.push_str("\u{fffd}");
                            self.utf8_tail.drain(..length);
                        }
                        // At most three bytes can await a scalar's completion.
                        None => return,
                    }
                }
            }
        }
    }

    fn emit_line(&mut self) -> io::Result<()> {
        if !self.utf8_tail.is_empty() {
            self.text.push_str("\u{fffd}");
            self.utf8_tail.clear();
        }
        let text = std::mem::take(&mut self.text).finish();
        self.has_pending = false;
        // The newline is host framing; line content, including the marker for
        // the entire omitted tail, is capped by the shared sanitizer.
        let result = writeln!(self.sink, "{text}").and_then(|()| self.sink.flush());
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        if !self.has_pending {
            self.flush_sink()
        } else {
            self.emit_line()
        }
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
    use carina_core::provider_text::ProviderText;
    use tokio::io::AsyncWriteExt;

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

    /// Unit-level coverage of the actual forwarding sink, without a compiled
    /// WASM fixture. Split writes also exercise shared line buffering.
    #[test]
    fn guest_stderr_secret_plaintext_is_redacted_before_forwarding() {
        const SECRET: &str = "guest-stderr-private-4305";
        const MASK: &str = "(secret)";
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(
            stream.as_mut(),
            b"provider: public-context-4305 password=guest-stderr-",
        );
        write_all(stream.as_mut(), b"private-4305\n");
        drop(stream);
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
