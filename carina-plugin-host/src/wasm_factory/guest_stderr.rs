//! Host-owned stderr for provider guests. Oversized guest lines are forcibly
//! terminated; subsequent bytes start a new bounded output line.

use std::io::{self, Write};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use carina_core::provider_text::{MAX_PROVIDER_TEXT_BYTES, ProviderText};
use hyper::body::Bytes;
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamError, StreamResult};

// A scalar can start just below the byte limit. Permit at most one scalar's
// lookahead so valid UTF-8 is not replaced just because a forced break falls
// inside it. Invalid input cannot keep growing this buffer indefinitely.
const MAX_PENDING_BYTES: usize = MAX_PROVIDER_TEXT_BYTES + char::MAX.len_utf8();

/// All handles for one guest stderr share a buffer and a host-owned sink.
#[derive(Clone)]
pub(super) struct GuestStderr {
    state: Arc<Mutex<LineBuffer>>,
}

struct LineBuffer {
    pending: Vec<u8>,
    sink: Box<dyn Write + Send>,
    failed: bool,
    just_broke_line: bool,
}

impl GuestStderr {
    pub(super) fn new(sink: impl Write + Send + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(LineBuffer {
                pending: Vec::with_capacity(MAX_PENDING_BYTES),
                sink: Box::new(sink),
                failed: false,
                just_broke_line: false,
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
                // An automatic break already supplied this delimiter. A
                // subsequent newline still produces an ordinary empty line.
                if !self.just_broke_line {
                    self.emit_line()?;
                }
                self.just_broke_line = false;
            } else {
                self.just_broke_line = false;
                self.pending.push(byte);
                if self.pending.len() > MAX_PROVIDER_TEXT_BYTES
                    && (self.pending.len() == MAX_PENDING_BYTES
                        || !incomplete_utf8_tail(&self.pending))
                {
                    self.emit_line()?;
                    self.just_broke_line = true;
                }
            }
        }
        Ok(())
    }

    fn emit_line(&mut self) -> io::Result<()> {
        let text = ProviderText::new(String::from_utf8_lossy(&self.pending).into_owned());
        self.pending.clear();
        // The newline is host framing; line content, including any truncation
        // marker, is capped by ProviderText. No raw guest bytes reach the sink.
        let result = writeln!(self.sink, "{text}").and_then(|()| self.sink.flush());
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        self.ensure_open()?;
        if self.pending.is_empty() {
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

fn incomplete_utf8_tail(mut bytes: &[u8]) -> bool {
    while let Err(error) = std::str::from_utf8(bytes) {
        match error.error_len() {
            Some(length) => bytes = &bytes[error.valid_up_to() + length..],
            None => return true,
        }
    }
    false
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
        assert!(
            capture.text().contains("… (truncated, "),
            "an overlong line must appear before a newline or drop"
        );
        assert!(stderr.state.lock().unwrap().pending.len() <= MAX_PROVIDER_TEXT_BYTES + 3);
        write_all(stream.as_mut(), b"\n");
        let text = capture.text();
        assert!(!text.as_bytes().contains(&0x1b));
        assert!(!text.as_bytes().contains(&0x07));
        assert!(
            text.lines()
                .all(|line| line.len() <= MAX_PROVIDER_TEXT_BYTES)
        );
        for line in text.lines().filter(|line| line.contains("… (truncated, ")) {
            let (prefix, marker) = line.split_once("… (truncated, ").unwrap();
            let omitted: usize = marker.strip_suffix(" bytes)").unwrap().parse().unwrap();
            assert_eq!(prefix.len() + omitted, MAX_PROVIDER_TEXT_BYTES + 1);
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
    fn guest_stderr_keeps_utf8_at_forced_line_boundaries_and_bounds_invalid_input() {
        for suffix in ["あ".as_bytes(), "🦀".as_bytes()] {
            let capture = Capture::default();
            let stderr = GuestStderr::new(capture.clone());
            let mut stream = stderr.p2_stream();
            write_all(stream.as_mut(), &vec![b'x'; MAX_PROVIDER_TEXT_BYTES - 1]);
            for byte in suffix {
                write_all(stream.as_mut(), &[*byte]);
                assert!(stderr.state.lock().unwrap().pending.len() <= MAX_PROVIDER_TEXT_BYTES + 3);
            }
            assert!(capture.text().contains("truncated,"));
            assert!(!capture.text().contains('\u{fffd}'));
        }

        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(stream.as_mut(), &vec![0xe2; 3 * MAX_PROVIDER_TEXT_BYTES]);
        assert!(stderr.state.lock().unwrap().pending.len() <= MAX_PROVIDER_TEXT_BYTES + 3);
        assert!(capture.text().contains('\u{fffd}'));
        assert!(
            capture
                .text()
                .lines()
                .all(|line| line.len() <= MAX_PROVIDER_TEXT_BYTES)
        );
    }

    #[test]
    fn guest_stderr_preserves_delimiters_after_forced_breaks_and_escapes_other_controls() {
        let capture = Capture::default();
        let stderr = GuestStderr::new(capture.clone());
        let mut stream = stderr.p2_stream();
        write_all(stream.as_mut(), &vec![b'x'; MAX_PROVIDER_TEXT_BYTES + 1]);
        assert_eq!(capture.text().lines().count(), 1);
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
