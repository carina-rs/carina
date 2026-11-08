//! Loopback servers with bounded accepts and blocking request streams.

use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_HTTP_REQUEST_BODY_BYTES: usize = 1024 * 1024;

/// A test listener that only hands out blocking streams.
///
/// Bind errors are returned so callers can report a skip when their sandbox
/// does not permit loopback servers. The underlying listener stays private.
pub struct LoopbackListener {
    listener: TcpListener,
}

/// A server whose accepts are armed immediately before guest dispatch.
pub struct ArmedLoopbackServer<T> {
    arms: mpsc::Sender<()>,
    thread: thread::JoinHandle<T>,
}

impl<T> ArmedLoopbackServer<T> {
    /// Allow one accept to begin with a fresh budget.
    pub fn arm(&self) -> io::Result<()> {
        self.arms
            .send(())
            .map_err(|error| io::Error::new(io::ErrorKind::BrokenPipe, error))
    }

    /// Close the arming channel so an unarmed server can finish, then join it.
    pub fn join(self) -> thread::Result<T> {
        drop(self.arms);
        self.thread.join()
    }
}

/// A listener that waits for an arm before starting each accept budget.
pub struct ArmedLoopbackListener {
    listener: LoopbackListener,
    arms: mpsc::Receiver<()>,
}

impl ArmedLoopbackListener {
    /// Return `None` when the server handle is dropped or joined without an arm.
    pub fn accept_within(&self, budget: Duration) -> io::Result<Option<TcpStream>> {
        self.listener
            .accept_when_ready(budget, || self.arms.recv().is_ok())
    }
}

/// Read one complete Content-Length-framed HTTP test request, with a body cap of 1 MiB.
pub fn read_http_request(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = read_http_request_head(stream)?;
    let head_length = request.len();
    let body_length = http_request_content_length(&request)?;
    if body_length > MAX_HTTP_REQUEST_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "request body exceeds the 1 MiB fixture cap",
        ));
    }
    let request_length = head_length.checked_add(body_length).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "request Content-Length is too large",
        )
    })?;
    request.resize(request_length, 0);
    stream.read_exact(&mut request[head_length..])?;
    Ok(request)
}

/// Read exactly the head, leaving the body unread for early-response probes.
pub fn read_http_request_head(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        head.push(byte[0]);
        if head.len() > 32 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head is too large",
            ));
        }
    }
    Ok(head)
}

/// Parse the Content-Length framing required by the HTTP probe fixtures.
pub fn http_request_content_length(head: &[u8]) -> io::Result<usize> {
    let head = std::str::from_utf8(head)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut length = None;
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let parsed = value
            .trim()
            .parse::<usize>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if length.is_some_and(|previous| previous != parsed) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request has conflicting Content-Length headers",
            ));
        }
        length = Some(parsed);
    }
    length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "request is missing Content-Length",
        )
    })
}

impl LoopbackListener {
    /// Bind an ephemeral IPv4 loopback port.
    pub fn bind() -> io::Result<Self> {
        Self::from_std(TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?)
    }

    /// Take ownership of a listener configured before listen/accept, for example
    /// with a small receive buffer to keep a large request body in flight.
    /// Non-loopback local addresses are rejected with [`io::ErrorKind::InvalidInput`].
    pub fn from_std(listener: TcpListener) -> io::Result<Self> {
        if !listener.local_addr()?.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "test listener must bind a loopback address",
            ));
        }
        listener.set_nonblocking(true)?;
        Ok(Self { listener })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Spawn a server that waits for an arm before each bounded accept.
    /// Dropping the returned handle closes the arming channel.
    pub fn spawn_armed<T: Send + 'static>(
        self,
        serve: impl FnOnce(ArmedLoopbackListener) -> T + Send + 'static,
    ) -> ArmedLoopbackServer<T> {
        let (arms, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            serve(ArmedLoopbackListener {
                listener: self,
                arms: receiver,
            })
        });
        ArmedLoopbackServer { arms, thread }
    }

    /// Start a fresh accept budget now and return a blocking stream.
    pub fn accept_within(&self, budget: Duration) -> io::Result<TcpStream> {
        self.accept_when_ready(budget, || true)
            .map(|stream| stream.expect("an unarmed listener is always ready"))
    }

    fn accept_when_ready(
        &self,
        budget: Duration,
        ready: impl FnOnce() -> bool,
    ) -> io::Result<Option<TcpStream>> {
        run_accept_within(
            budget,
            ready,
            Instant::now,
            || {
                self.listener.accept().and_then(|(stream, _)| {
                    // macOS/BSD can inherit O_NONBLOCK from the listener, which
                    // makes read timeouts ineffective until blocking is restored.
                    stream.set_nonblocking(false)?;
                    Ok(stream)
                })
            },
            thread::sleep,
        )
    }
}

fn run_accept_within<T, C>(
    budget: Duration,
    ready: impl FnOnce() -> bool,
    mut now: impl FnMut() -> C,
    mut accept: impl FnMut() -> io::Result<T>,
    mut pause: impl FnMut(Duration),
) -> io::Result<Option<T>>
where
    C: Copy + Ord + std::ops::Add<Duration, Output = C>,
{
    if !ready() {
        return Ok(None);
    }
    let deadline = now() + budget;
    loop {
        match accept() {
            Ok(stream) => return Ok(Some(stream)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // wall-clock-ok: hang guard for the real loopback server accept.
                if now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "server accept timed out",
                    ));
                }
                pause(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::mpsc;

    #[test]
    fn accept_budget_starts_after_arm() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let clock = Arc::new(AtomicU64::new(0));
        let server_clock = clock.clone();
        let (arm, wait_for_arm) = mpsc::channel();
        let (waiting, wait_for_waiting) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let mut attempts = 0;
                requests.push(run_accept_within(
                    Duration::from_secs(30),
                    || {
                        waiting.send(()).expect("signal waiting for an arm");
                        wait_for_arm.recv().is_ok()
                    },
                    || Duration::from_secs(server_clock.load(Ordering::SeqCst)),
                    || {
                        attempts += 1;
                        if attempts == 1 {
                            server_clock.fetch_add(1, Ordering::SeqCst);
                            Err(io::ErrorKind::WouldBlock.into())
                        } else {
                            Ok("request")
                        }
                    },
                    |_| {},
                )?);
            }
            Ok::<_, io::Error>(requests)
        });

        for setup_finished_at in [1000, 2000] {
            // wall-clock-ok: hang guard for the server to reach its arming handshake.
            wait_for_waiting
                .recv_timeout(Duration::from_secs(2))
                .expect("server must wait for an arm");
            // Simulate each guest setup taking much longer than its accept budget.
            clock.store(setup_finished_at, Ordering::SeqCst);
            arm.send(()).expect("arm after guest setup");
        }

        assert_eq!(
            server.join().expect("server must finish").unwrap(),
            vec![Some("request"), Some("request")]
        );
    }

    #[test]
    fn joining_unarmed_server_cancels_accept_without_starting_budget() {
        let (arms, receiver) = mpsc::channel();
        let server = ArmedLoopbackServer {
            arms,
            thread: thread::spawn(move || {
                run_accept_within(
                    Duration::from_secs(30),
                    || receiver.recv().is_ok(),
                    || -> Duration { panic!("an unarmed server must not start its budget") },
                    || Ok("request"),
                    |_| {},
                )
            }),
        };

        assert_eq!(server.join().expect("server must finish").unwrap(), None);
    }

    #[test]
    fn read_http_request_rejects_body_above_fixture_cap() {
        let head = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_HTTP_REQUEST_BODY_BYTES + 1
        );
        let mut stream = Cursor::new([head.as_bytes(), b"body"].concat());

        assert_eq!(
            read_http_request(&mut stream).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(stream.position(), u64::try_from(head.len()).unwrap());
    }

    #[test]
    fn read_http_request_rejects_conflicting_content_lengths() {
        let mut stream =
            &b"POST / HTTP/1.1\r\nContent-Length: 3\r\ncontent-length: 4\r\n\r\nabcd"[..];

        assert_eq!(
            read_http_request(&mut stream).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn read_http_request_accepts_matching_content_lengths() {
        let request = b"POST / HTTP/1.1\r\nContent-Length: 6\r\ncontent-length: 6\r\n\r\nabcdef";

        assert_eq!(read_http_request(&mut request.as_slice()).unwrap(), request);
    }

    #[test]
    fn from_std_rejects_non_loopback_address() {
        let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot bind the wildcard test listener: {error}");
                return;
            }
        };

        let error = LoopbackListener::from_std(listener)
            .err()
            .expect("wildcard listener must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn read_http_request_waits_for_fragmented_head_and_body() {
        use std::os::unix::net::UnixStream;

        struct AcknowledgedReader {
            stream: UnixStream,
            consumed: mpsc::Sender<usize>,
        }

        impl Read for AcknowledgedReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                let count = self.stream.read(buffer)?;
                self.consumed
                    .send(count)
                    .expect("acknowledge request bytes");
                Ok(count)
            }
        }

        let (server, mut client) = UnixStream::pair().expect("create request stream pair");
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");
        let (consumed, wait_for_consumed) = mpsc::channel();
        let mut reader = AcknowledgedReader {
            stream: server,
            consumed,
        };
        let writer = std::thread::spawn(move || {
            let fragments: &[&[u8]] = &[
                b"POST / HTTP/1.1\r\nCon",
                b"tent-Length: 6\r\n\r",
                b"\nabc",
                b"def",
            ];
            for fragment in fragments {
                client.write_all(fragment).expect("write request fragment");
                let mut pending = fragment.len();
                while pending > 0 {
                    // wall-clock-ok: hang guard for the reader to consume this fragment before the next write.
                    let count = wait_for_consumed
                        .recv_timeout(Duration::from_secs(2))
                        .expect("reader must consume the fragment before the next write");
                    assert!(count > 0, "reader must consume request bytes");
                    pending -= count;
                }
            }
        });

        let request = read_http_request(&mut reader).expect("read fragmented request");
        assert_eq!(
            request,
            b"POST / HTTP/1.1\r\nContent-Length: 6\r\n\r\nabcdef"
        );
        writer.join().expect("request writer must finish");
    }

    #[test]
    fn read_http_request_reads_exact_content_length() {
        let expected = b"POST / HTTP/1.1\r\ncOnTeNt-LeNgTh: 6\r\n\r\nabcdef";
        let mut stream = Cursor::new([expected.as_slice(), b"next request"].concat());

        assert_eq!(read_http_request(&mut stream).unwrap(), expected);
        let mut remaining = Vec::new();
        stream.read_to_end(&mut remaining).unwrap();
        assert_eq!(remaining, b"next request");
    }

    #[test]
    fn read_http_request_rejects_incomplete_body() {
        let mut stream = &b"POST / HTTP/1.1\r\nContent-Length: 6\r\n\r\nabc"[..];

        assert_eq!(
            read_http_request(&mut stream).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn first_read_waits_for_client_bytes() {
        let listener = match LoopbackListener::bind() {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("SKIP: sandbox cannot bind the loopback test server: {error}");
                return;
            }
        };
        let address = listener.local_addr().expect("loopback listener address");
        let (connected, wait_for_connection) = mpsc::channel();
        let (about_to_read, wait_for_read) = mpsc::channel();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect loopback client");
            connected.send(()).expect("signal client connection");
            // wall-clock-ok: hang guard for the server to accept and begin reading.
            wait_for_read
                .recv_timeout(Duration::from_secs(5))
                .expect("server must signal before the client sends");
            stream.write_all(b"x").expect("send request byte");
        });

        // wall-clock-ok: hang guard for the real loopback client connection.
        wait_for_connection
            .recv_timeout(Duration::from_secs(5))
            .expect("client must connect before accept");
        let mut stream = listener
            .accept_within(Duration::from_secs(5))
            .expect("accept loopback client");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server read timeout");

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            // Check before releasing the writer so scheduling cannot hide an
            // inherited O_NONBLOCK flag by delivering bytes before read starts.
            // SAFETY: stream owns a live descriptor and F_GETFL takes no extra argument.
            let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1, "read accepted socket flags");
            assert_eq!(
                flags & libc::O_NONBLOCK,
                0,
                "accepted socket must be blocking"
            );
        }

        about_to_read.send(()).expect("release client writer");
        let mut request = [0_u8; 1];
        assert_eq!(
            stream
                .read(&mut request)
                .expect("first read must wait for bytes"),
            1
        );
        assert_eq!(&request, b"x");
        client.join().expect("loopback client must finish");
    }
}
