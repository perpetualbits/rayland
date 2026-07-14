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
            // A closed connection is end-of-stream for our synchronous consumers: the client's
            // liveness read treats "server closed the connection" as a clean finish, and the
            // server's command read treats "client vanished" as EOF. Other read errors remain
            // hard errors.
            Err(quinn::ReadError::ConnectionLost(_)) => Ok(0),
            // A read error (reset, other connection errors) becomes an io::Error.
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
            // A closed connection is end-of-stream for our synchronous consumers: the client's
            // liveness read treats "server closed the connection" as a clean finish, and the
            // server's command read treats "client vanished" as EOF. Other read errors remain
            // hard errors.
            Err(quinn::ReadError::ConnectionLost(_)) => Ok(0),
            Err(e) => Err(std::io::Error::other(e)),
        }
    }
}

/// A liveness handle for the server's window loop.
///
/// It bundles three things the SP1 window loop needs, now transport-agnostic:
/// - an fd ([`AsFd`]) that reaches **end-of-file when the client disconnects**, so the calloop
///   loop can watch it exactly as SP1 watched the TCP socket fd (a background task closes the
///   pipe's write end when the QUIC connection closes);
/// - the server's send half of the client's bi-stream, **held open and never written**, so the
///   client's receive half does not reach EOF the moment the stream is accepted — quinn's
///   `SendStream::Drop` sends a clean FIN, so dropping it early would end the client's liveness
///   read prematurely;
/// - a `Drop` that **closes the QUIC connection**, so when the window is closed and the loop
///   drops this handle, the client's stream observes the close and the client exits.
///
/// This preserves SP1's "close on either" teardown over QUIC.
pub struct Liveness {
    // Keeps the runtime alive so the background close-watcher task keeps running.
    _rt: Arc<Runtime>,
    // The QUIC connection; closed on drop (window-close → client sees EOF).
    conn: quinn::Connection,
    // The server->client send half, held open (never written) so the client's receive half
    // stays open until we deliberately close the connection. Dropping it early would send a
    // FIN and make the client see EOF immediately.
    _send: quinn::SendStream,
    // The read end of a pipe; reaches EOF when the watcher closes the write end on disconnect.
    disconnect: File,
}

impl Liveness {
    /// Build a liveness handle: spawn a task that closes `pipe_write` when `conn` closes, and
    /// keep the pipe read end for the caller to watch. `send` is the server's half of the
    /// client's bi-stream; it is held open (never written) for the lifetime of this handle so
    /// the client's receive half does not reach EOF until we deliberately close the connection
    /// on drop. `disconnect` must be non-blocking.
    pub(crate) fn new(
        rt: Arc<Runtime>,
        conn: quinn::Connection,
        send: quinn::SendStream,
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
            _send: send,
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
    /// Close the QUIC connection when the window loop drops us (window closed → client's read
    /// unblocks). The peer observes this as a `ConnectionLost` error, which the read adapters
    /// in this module translate to end-of-file (`Ok(0)`) — see `QuicStream::read`.
    fn drop(&mut self) {
        // Application close code 0 with a short reason; the client's read then fails with
        // `ReadError::ConnectionLost`, which `QuicStream::read` maps to EOF.
        self.conn.close(0u32.into(), b"window closed");
        // `close` only *queues* the CONNECTION_CLOSE frame; the runtime's worker still has to
        // transmit it. If we returned now, the process could drop the runtime before that
        // happens, and the client would fall back to the ~5s idle timeout instead of exiting
        // promptly. Block (on this non-runtime thread; the runtime is still alive via `_rt`)
        // until the connection has finished closing, so the close frame is on the wire before
        // we proceed. This resolves quickly for a locally-initiated close.
        let _ = self._rt.block_on(self.conn.closed());
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
        let (conn, send, recv) = self.rt.block_on(async {
            // Wait for an incoming connection and finish its handshake.
            let incoming =
                self.endpoint.accept().await.ok_or_else(|| {
                    anyhow::anyhow!("endpoint closed before a connection arrived")
                })?;
            let conn = incoming.await?;
            // Accept the client's single command bi-stream. We only read from `recv`; `send` is
            // not written to but must be kept alive (not dropped) by the caller — see
            // `Liveness`'s doc comment for why dropping it here would end the client's liveness
            // read prematurely with an unintended FIN.
            let (send, recv) = conn.accept_bi().await?;
            anyhow::Ok((conn, send, recv))
        })?;

        // Create a non-blocking pipe: the read end is watched by the window loop; the write end
        // is closed by Liveness's background task when the connection closes.
        let (read_fd, write_fd) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::NONBLOCK)?;
        // Convert the OwnedFds into std Files for convenient Read + AsFd.
        let disconnect = File::from(read_fd);
        let pipe_write = File::from(write_fd);

        // Build the reader and liveness handle sharing this listener's runtime.
        let quic_recv = QuicRecv::new(self.rt.clone(), recv);
        let liveness = Liveness::new(self.rt.clone(), conn, send, disconnect, pipe_write);
        Ok((quic_recv, liveness))
    }
}

#[cfg(test)]
mod sync_tests {
    use crate::{connect, listen};
    use std::io::{Read, Write};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    // Prove the public sync API end-to-end in two parts:
    //
    // (a) the data path: bytes written by the client's `Write` arrive at the server's `Read`.
    //     The client finishes its OWN send half to signal end-of-command-stream — a legitimate
    //     client->server FIN, unrelated to liveness teardown.
    //
    // (b) the liveness path: the server holds `Liveness` open (does NOT drop it, does NOT
    //     finish its send half except through `Liveness`'s `Drop`) while the client's liveness
    //     read (`read_to_end`, mirroring the window loop's disconnect-watch idiom) is proven to
    //     still be pending. Only once the test explicitly drops `liveness` on the server side
    //     does the client's read unblock and observe EOF. This is the genuine regression test
    //     for holding the server's send half open (sync_stream.rs `Liveness::_send`): if that
    //     field were removed and `accept()` went back to dropping the send half immediately,
    //     the client's read would complete long before `liveness` is dropped, and the
    //     `still_pending` assertion below would fail.
    #[test]
    fn sync_api_round_trips_and_signals_disconnect() {
        // Bind the server on an ephemeral localhost port.
        let listener = listen(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)).expect("listen");
        // Discover the bound address for the client (port 0 was resolved by the OS).
        let addr = listener.local_addr().expect("listener local addr");

        // The server uses this to tell the main thread "I've read the payload and am now
        // holding `liveness` open, doing nothing else to the connection."
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel::<()>();
        // The main thread uses this to tell the server "drop `liveness` now" — the ONLY
        // event in this test that is allowed to close the client's receive half.
        let (drop_tx, drop_rx) = std::sync::mpsc::channel::<()>();

        // Server thread: accept, read the client's bytes to EOF (client-initiated FIN), signal
        // the main thread, then block until told to drop `liveness` (closing the connection).
        let server = std::thread::spawn(move || {
            let (mut recv, liveness) = listener.accept().expect("accept");
            // Read everything the client sends until it half-closes its own send side.
            let mut got = Vec::new();
            recv.read_to_end(&mut got).expect("read to end");
            // Tell the main thread the payload is in and `liveness` is now being held open
            // untouched, so it can safely start (and time-bound) the client's liveness read.
            accepted_tx.send(()).expect("signal accepted");
            // Do not drop `liveness` until explicitly told to; this is what keeps the
            // server->client send half open per Change 1, and is exactly what the test below
            // exercises.
            drop_rx.recv().expect("wait for drop signal");
            drop(liveness);
            // `Liveness::drop` only QUEUES the CONNECTION_CLOSE frame and wakes the
            // connection's background driver task (see `sync_stream.rs`'s `Liveness::_send`
            // doc comment); the frame is actually put on the wire whenever the runtime's
            // worker threads next get to poll that task. `listener` and `recv` are the only
            // remaining owners of this test's `Arc<Runtime>` clones, and both are about to be
            // dropped when this closure returns — which would tear down every worker thread
            // immediately. Without this pause, that teardown races the driver task: on an
            // unlucky scheduling, the runtime (and its worker threads) can be gone before the
            // close frame is ever transmitted, so the client would never see it and would fall
            // all the way back to QUIC's 30s idle timeout instead of observing the close
            // promptly. Sleeping here gives the driver task a guaranteed window to run while
            // the runtime is still alive.
            std::thread::sleep(std::time::Duration::from_millis(100));
            got
        });

        // Client: connect, send bytes, finish the send side so the server sees EOF.
        let mut stream = connect(addr).expect("connect");
        stream.write_all(b"hello quic").expect("write");
        // Signal end of the client's send stream so the server's read_to_end returns. This is
        // the client's own FIN and is unrelated to liveness teardown.
        stream.finish().expect("finish");

        // Wait for the server to have consumed the payload and be holding `liveness` open.
        accepted_rx
            .recv()
            .expect("server accepted and is holding liveness");

        // Perform the client's liveness read (the window-loop's disconnect-watch idiom) on its
        // own thread so the main thread can observe whether it is still pending.
        let (tail_tx, tail_rx) = std::sync::mpsc::channel();
        let read_thread = std::thread::spawn(move || {
            let mut tail = Vec::new();
            stream
                .read_to_end(&mut tail)
                .expect("client reads to EOF after server drops liveness");
            tail_tx.send(tail).expect("send tail");
        });

        // The read must still be pending: nothing has closed the connection yet. If the
        // server's send half were dropped at accept time (Change 1 reverted), this read would
        // already have completed and `recv_timeout` would return `Ok` instead of timing out.
        let still_pending = tail_rx.recv_timeout(std::time::Duration::from_millis(200));
        assert!(
            still_pending.is_err(),
            "client's liveness read returned before the server dropped `liveness` -- \
             the server's send half was not held open (Change 1 regressed)"
        );

        // Now let the server drop `liveness`, closing the connection — the sole trigger for
        // the client's read to unblock.
        drop_tx.send(()).expect("signal drop");

        // The server received exactly what we sent.
        let got = server.join().expect("server thread");
        assert_eq!(got, b"hello quic");

        // After the server dropped `liveness`, the client's read reaches EOF (`ConnectionLost`
        // mapped to `Ok(0)` by `QuicStream::read`, Change 4).
        let tail = tail_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("client's liveness read unblocked after liveness was dropped");
        // No trailing bytes were sent after the payload; EOF should be immediate.
        assert!(tail.is_empty());

        read_thread.join().expect("read thread");
    }
}
