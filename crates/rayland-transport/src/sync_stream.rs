//! Blocking `Read`/`Write` adapters over asynchronous `quinn` streams.
//!
//! Each adapter holds a handle to the confined tokio runtime (which drives QUIC on its own
//! threads) and calls `block_on` per operation from the caller's synchronous thread. Because
//! the caller runs on a NON-runtime thread while the runtime's workers keep driving the
//! connection, `block_on` blocks only the caller — no deadlock. Calling these from inside a
//! runtime worker thread would panic; callers must stay on their own thread.

// Standard IO traits we implement.
use std::io::{Read, Write};
// The pipe read-end that signals disconnect, and fd borrowing for calloop.
use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
// The confined runtime shared by all adapters.
use tokio::runtime::Runtime;

/// A synchronous, bidirectional stream over one QUIC bi-stream (the client's view).
pub struct QuicStream {
    // Keeps the runtime alive for the stream's lifetime; drives the async reads/writes.
    rt: Arc<Runtime>,
    // The QUIC send half.
    send: quinn::SendStream,
    // The QUIC receive half.
    recv: quinn::RecvStream,
}

impl QuicStream {
    /// Wrap a QUIC bi-stream pair with the runtime handle.
    pub(crate) fn new(rt: Arc<Runtime>, send: quinn::SendStream, recv: quinn::RecvStream) -> Self {
        Self { rt, send, recv }
    }

    /// Finish the client's send half of the stream: no more bytes will be written.
    ///
    /// This is how the client signals "no more commands" to the peer's blocking reader (its
    /// `read`/`read_to_end` observes EOF once the finish is acknowledged by QUIC). quinn's
    /// `SendStream::finish` is synchronous (it only marks local state and queues a QUIC frame;
    /// it does not wait for peer acknowledgement), so no `block_on` is needed, but we route it
    /// through the runtime handle for symmetry with the rest of this type's API and in case a
    /// future quinn version makes it asynchronous again.
    ///
    /// # Errors
    /// Returns an error if the stream was already finished or reset.
    pub fn finish(&mut self) -> std::io::Result<()> {
        // `finish` returns `Result<(), ClosedStream>`; map to `io::Error` like the other ops.
        self.send.finish().map_err(std::io::Error::other)
    }
}

impl Read for QuicStream {
    /// Block until some bytes arrive; a finished stream reads as end-of-file (`Ok(0)`).
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drive the async read to completion on the runtime.
        match self.rt.block_on(self.recv.read(buf)) {
            // Some bytes were read.
            Ok(Some(n)) => Ok(n),
            // The stream finished cleanly: report EOF so sync readers terminate.
            Ok(None) => Ok(0),
            // A read error (reset, connection lost) becomes an io::Error.
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

impl Write for QuicStream {
    /// Block until the bytes are accepted by the QUIC stream.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // `write` returns how many bytes were accepted.
        self.rt
            .block_on(self.send.write(buf))
            .map_err(std::io::Error::other)
    }
    /// QUIC streams are not user-buffered here, so flush is a no-op success.
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The server's synchronous view of the incoming command stream (receive half only).
pub struct QuicRecv {
    // Keeps the runtime alive and drives the reads.
    rt: Arc<Runtime>,
    // The QUIC receive half of the client's bi-stream.
    recv: quinn::RecvStream,
}

impl QuicRecv {
    /// Wrap a QUIC receive stream with the runtime handle.
    pub(crate) fn new(rt: Arc<Runtime>, recv: quinn::RecvStream) -> Self {
        Self { rt, recv }
    }
}

impl Read for QuicRecv {
    /// Same semantics as [`QuicStream::read`]: finished stream → EOF.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self.rt.block_on(self.recv.read(buf)) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Ok(0),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

/// A liveness handle for the server's window loop.
///
/// It bundles two things the SP1 window loop needs, now transport-agnostic:
/// - an fd ([`AsFd`]) that reaches **end-of-file when the client disconnects**, so the calloop
///   loop can watch it exactly as SP1 watched the TCP socket fd (a background task closes the
///   pipe's write end when the QUIC connection closes);
/// - a `Drop` that **closes the QUIC connection**, so when the window is closed and the loop
///   drops this handle, the client's stream reaches EOF and the client exits.
///
/// This preserves SP1's "close on either" teardown over QUIC.
pub struct Liveness {
    // Keeps the runtime alive so the background close-watcher task keeps running.
    _rt: Arc<Runtime>,
    // The QUIC connection; closed on drop (window-close → client sees EOF).
    conn: quinn::Connection,
    // The read end of a pipe; reaches EOF when the watcher closes the write end on disconnect.
    disconnect: File,
}

impl Liveness {
    /// Build a liveness handle: spawn a task that closes `pipe_write` when `conn` closes, and
    /// keep the pipe read end for the caller to watch. `disconnect` must be non-blocking.
    pub(crate) fn new(
        rt: Arc<Runtime>,
        conn: quinn::Connection,
        disconnect: File,
        pipe_write: File,
    ) -> Self {
        // Watch the connection for close and signal the pipe by dropping its write end.
        let watch_conn = conn.clone();
        rt.spawn(async move {
            // Resolves when the peer (client) disconnects or the connection otherwise closes.
            watch_conn.closed().await;
            // Dropping the write end makes the read end report EOF to the window loop.
            drop(pipe_write);
        });
        Self {
            _rt: rt,
            conn,
            disconnect,
        }
    }
}

impl AsFd for Liveness {
    /// Expose the disconnect pipe's read end so the window loop can poll it.
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.disconnect.as_fd()
    }
}

impl Read for &Liveness {
    /// Read from the disconnect pipe (used by the window loop's calloop callback). Returns
    /// `Ok(0)` (EOF) once the client has disconnected.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // `&File` implements Read; delegate to the pipe read end.
        (&self.disconnect).read(buf)
    }
}

impl Drop for Liveness {
    /// Close the QUIC connection when the window loop drops us (window closed → client EOF).
    fn drop(&mut self) {
        // Application close code 0 with a short reason; the client observes this as EOF.
        self.conn.close(0u32.into(), b"window closed");
    }
}

/// A QUIC listener that accepts one client connection at a time.
pub struct QuicListener {
    // The runtime driving the endpoint; shared into accepted connections' adapters.
    rt: Arc<Runtime>,
    // The bound QUIC server endpoint.
    endpoint: quinn::Endpoint,
}

impl QuicListener {
    /// Wrap a bound server endpoint.
    pub(crate) fn new(rt: Arc<Runtime>, endpoint: quinn::Endpoint) -> Self {
        Self { rt, endpoint }
    }

    /// Report the local address the endpoint is bound to (useful when `bind_addr`'s port was
    /// `0`, letting the OS choose an ephemeral port — callers need the actual port to connect).
    ///
    /// # Errors
    /// Returns an error if the underlying UDP socket cannot report its local address.
    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Accept one connection and its bidirectional command stream; return a synchronous reader
    /// for the stream and a [`Liveness`] handle for the window loop.
    ///
    /// # Errors
    /// Returns an error if accepting the connection or the bi-stream fails, or if the disconnect
    /// pipe cannot be created.
    pub fn accept(&self) -> anyhow::Result<(QuicRecv, Liveness)> {
        // Accept the connection and its first bi-stream on the runtime.
        let (conn, recv) = self.rt.block_on(async {
            // Wait for an incoming connection and finish its handshake.
            let incoming =
                self.endpoint.accept().await.ok_or_else(|| {
                    anyhow::anyhow!("endpoint closed before a connection arrived")
                })?;
            let conn = incoming.await?;
            // Accept the client's single command bi-stream; we only read from it.
            let (_send, recv) = conn.accept_bi().await?;
            anyhow::Ok((conn, recv))
        })?;

        // Create a non-blocking pipe: the read end is watched by the window loop; the write end
        // is closed by Liveness's background task when the connection closes.
        let (read_fd, write_fd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK)?;
        // Convert the OwnedFds into std Files for convenient Read + AsFd.
        let disconnect = File::from(read_fd);
        let pipe_write = File::from(write_fd);

        // Build the reader and liveness handle sharing this listener's runtime.
        let quic_recv = QuicRecv::new(self.rt.clone(), recv);
        let liveness = Liveness::new(self.rt.clone(), conn, disconnect, pipe_write);
        Ok((quic_recv, liveness))
    }
}

#[cfg(test)]
mod sync_tests {
    use crate::{connect, listen};
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // Prove the public sync API end-to-end: bytes written by the client's `Write` arrive at the
    // server's `Read`, and dropping the server's `Liveness` (the window-close analogue) makes
    // the client's `Read` observe EOF — the same teardown SP1 relied on over TCP.
    #[test]
    fn sync_api_round_trips_and_signals_disconnect() {
        // Bind the server on an ephemeral localhost port.
        let listener = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).expect("listen");
        // Discover the bound address for the client (port 0 was resolved by the OS).
        let addr = listener.local_addr().expect("listener local addr");

        // Server thread: accept, read the client's bytes to EOF, then drop liveness (closing conn).
        let server = std::thread::spawn(move || {
            let (mut recv, liveness) = listener.accept().expect("accept");
            // Read everything the client sends until it half-closes its send side.
            let mut got = Vec::new();
            recv.read_to_end(&mut got).expect("read to end");
            // Dropping `liveness` here closes the connection (window-close analogue).
            drop(liveness);
            got
        });

        // Client: connect, send bytes, finish the send side so the server sees EOF.
        let mut stream = connect(addr).expect("connect");
        stream.write_all(b"hello quic").expect("write");
        // Signal end of the client's send stream so the server's read_to_end returns.
        stream.finish().expect("finish");

        // The server received exactly what we sent.
        let got = server.join().expect("server thread");
        assert_eq!(got, b"hello quic");

        // After the server dropped liveness, the client's read reaches EOF.
        let mut tail = Vec::new();
        stream
            .read_to_end(&mut tail)
            .expect("client reads to EOF after server close");
        // No trailing bytes were sent after the payload; EOF should be immediate.
        assert!(tail.is_empty());
    }
}
