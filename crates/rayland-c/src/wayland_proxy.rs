//! WP0 — **the C-side Wayland proxy.**
//!
//! The application on C presents its window through Wayland. In Rayland's model that window is shown on
//! **S**, not C, so the application must not talk to a real compositor on C — it talks to *this* proxy,
//! which forwards its Wayland protocol to S (where a client replays it against S's real compositor) and
//! intercepts the one thing that cannot cross a network: the swapchain buffer's file descriptor, which is
//! replaced by a **buffer-by-token** naming the S-side resource the command relay already rendered.
//!
//! # Why the low-level `wayland_server::backend`, not the high-level typed API
//! A proxy forwards *whatever* the application does; it is not a compositor with its own object logic. So
//! it works at the structured-message layer: `wayland_backend` (re-exported as `wayland_server::backend`)
//! delivers each request as a `Message { sender_id, opcode, args }` whose `Argument`s the proxy translates
//! to a [`rayland_relay::WaylandMessage`] and forwards. The library owns all wire serialization and fd
//! plumbing; the proxy owns forwarding, the object-id bookkeeping, and the single fd→token interception.
//! See `docs/design/2026-07-22-wp0-wayland-proxy-first-light.md` §3 "Forwarding model".
//!
//! **Status: globals + connect (WP0 Task 3b, sub-step 2).** This stands up the backend, advertises the
//! minimal set of globals vkcube binds (§6 of the spec), and runs the accept-and-dispatch loop so the
//! application can connect and bind them. Request forwarding to S and the fd→token interception are the
//! following sub-steps; until then each bound object carries an inert [`ProxyObjectData`] that only tracks
//! the new-object bookkeeping the backend contract requires.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use wayland_server::Resource; // brings `interface()` into scope for the generated object types
use wayland_server::backend::protocol::{Argument, Message};
use wayland_server::backend::{
    Backend, ClientData, ClientId, GlobalHandler, GlobalId, Handle, ObjectData, ObjectId,
};

// The generated interface descriptors for the globals WP0 advertises. Each is only used to name the
// interface (its `&'static Interface`) when creating the global; the proxy never builds typed objects.
use wayland_protocols::wp::linux_dmabuf::zv1::server::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase;
use wayland_server::protocol::wl_compositor::WlCompositor;
use wayland_server::protocol::wl_seat::WlSeat;

/// The dispatch state the backend threads through every callback (`ObjectData::request`,
/// `GlobalHandler::bind`). It will grow to hold the link to S (to forward `C2S::WaylandRequest`), the
/// resource→memfd correlation used at buffer creation, and the `app_id ↔ s_id` bookkeeping. Empty in the
/// globals+connect sub-step: nothing is forwarded yet, so there is no shared state to thread.
struct ProxyState;

/// Per-connection state the backend holds for the application's client. Empty for now — WP0 has exactly
/// one client (the app) and needs no per-client bookkeeping beyond the object map a later sub-step adds.
/// The `initialized`/`disconnected` notifications are left at their trait defaults.
struct ProxyClientData;
impl ClientData for ProxyClientData {}

/// The handler the backend invokes when a client binds one of our advertised globals.
///
/// A `wayland-backend` global is bound via `wl_registry.bind`; the backend then calls [`Self::bind`] to
/// obtain the [`ObjectData`] for the freshly created global object (the app's `wl_compositor`,
/// `xdg_wm_base`, etc.). One handler instance is created per advertised global; it carries the interface's
/// human name purely so the bring-up log can say *which* global was bound.
struct ProxyGlobal {
    /// The interface name (`"wl_compositor"`, …) — diagnostic only; the backend already knows the binding.
    iface_name: &'static str,
}

impl GlobalHandler<ProxyState> for ProxyGlobal {
    /// A client has bound this global, creating `object_id`. Return the object data that will receive the
    /// bound object's future requests. In this sub-step that data is inert — it does the new-object
    /// bookkeeping the backend contract requires but forwards nothing.
    fn bind(
        self: Arc<Self>,
        _handle: &Handle,
        _data: &mut ProxyState,
        _client_id: ClientId,
        _global_id: GlobalId,
        object_id: ObjectId,
    ) -> Arc<dyn ObjectData<ProxyState>> {
        // Record the bind so bring-up can confirm vkcube reaches every WP0 global. Gated so a normal run
        // is silent; `RAYLAND_WP_LOG=1` turns it on.
        wp_log(&format!(
            "bound global {} -> object {}",
            self.iface_name,
            object_id.protocol_id()
        ));
        // The bound object forwards its requests through the same inert data as any other object for now.
        Arc::new(ProxyObjectData)
    }
}

/// The per-object request hook. Every Wayland object the app creates — the globals it binds and every
/// object made from them — carries an instance of this. In the globals+connect sub-step it does not yet
/// forward to S; it only honours the backend's new-object contract (see [`Self::request`]).
struct ProxyObjectData;

impl ObjectData<ProxyState> for ProxyObjectData {
    /// Dispatch one request from the application.
    ///
    /// # The new-object contract
    /// If the request carries a `NewId` argument (the app is creating an object — `create_surface`,
    /// `get_xdg_surface`, `create_params`, …), this method **must** return the [`ObjectData`] for that new
    /// object, or the backend has no handler for it. We hand back another inert [`ProxyObjectData`] so the
    /// whole object graph the app builds is covered. A request with no `NewId` returns `None`.
    ///
    /// Forwarding the request to S, translating its arguments, and the `params.add` fd→token interception
    /// are the next sub-step; here the request is observed (optionally logged) and dropped.
    fn request(
        self: Arc<Self>,
        _handle: &Handle,
        _data: &mut ProxyState,
        _client_id: ClientId,
        msg: Message<ObjectId, std::os::fd::OwnedFd>,
    ) -> Option<Arc<dyn ObjectData<ProxyState>>> {
        // Does this request create a new object? The backend needs data for it if so.
        let makes_new_object = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(_)));
        wp_log(&format!(
            "request obj {} opcode {} ({} args){}",
            msg.sender_id.protocol_id(),
            msg.opcode,
            msg.args.len(),
            if makes_new_object { " [new-id]" } else { "" }
        ));
        // Honour the contract: return handler data exactly when a new object was created.
        makes_new_object.then(|| Arc::new(ProxyObjectData) as Arc<dyn ObjectData<ProxyState>>)
    }

    /// The object was destroyed. Nothing to release yet (no per-object state); a later sub-step drops the
    /// object from the `app_id ↔ s_id` map here.
    fn destroyed(
        self: Arc<Self>,
        _handle: &Handle,
        _data: &mut ProxyState,
        _client_id: ClientId,
        _object_id: ObjectId,
    ) {
    }
}

/// Run the Wayland proxy: advertise the WP0 globals, bind the socket the application connects to, and
/// drive the backend so the application can connect and bind those globals.
///
/// # Inputs / outputs
/// - `socket_path`: the Unix socket the application's `WAYLAND_DISPLAY` names; where the app dials in.
///   Bound fresh — a stale socket file from a previous run is removed first so the bind cannot fail on
///   `EADDRINUSE` for a path nothing is listening on.
/// - Never returns under normal operation: it runs the accept-and-dispatch loop until the process exits or
///   an unrecoverable I/O error occurs (then returns that error).
///
/// # Failure modes
/// - The backend cannot be created (`wayland_server::backend::InitError`) — surfaced as an error.
/// - The socket cannot be bound (path unwritable, or a live listener already owns it) — surfaced as an
///   `io::Error`.
/// - `poll(2)` fails other than by interruption (`EINTR`, which is retried) — surfaced as an `io::Error`.
pub fn run(socket_path: PathBuf) -> anyhow::Result<()> {
    // A stale socket file left by a crashed run would make `bind` fail; remove it (ignore "not found").
    let _ = std::fs::remove_file(&socket_path);
    // The pure-Rust Wayland server backend. `ProxyState` is the dispatch state it threads to callbacks.
    let mut backend = Backend::<ProxyState>::new()?;
    // Advertise every global vkcube binds (spec §6). `wl_display`/`wl_registry` are backend built-ins; we
    // add only the application-visible globals. Each is advertised at the full version the interface
    // descriptor knows — WP0 negotiates no version subset yet.
    let handle = backend.handle();
    create_global::<WlCompositor>(&handle);
    create_global::<XdgWmBase>(&handle);
    create_global::<ZwpLinuxDmabufV1>(&handle);
    create_global::<WlSeat>(&handle);
    // Where the application dials in. WP0 serves exactly one such socket.
    let listener = UnixListener::bind(&socket_path)?;
    // The dispatch loop owns this state; it is threaded to every callback by the backend.
    let mut state = ProxyState;
    wp_log(&format!("proxy listening at {}", socket_path.display()));
    serve(&mut backend, &listener, &mut state)
}

/// Advertise one global for interface `R`, wiring it to a fresh [`ProxyGlobal`] handler.
///
/// `R` is a generated object type (`WlCompositor`, `XdgWmBase`, …); `R::interface()` yields the
/// `&'static Interface` the backend needs, and its `.version` is the maximum version the descriptor
/// supports, which is what we advertise.
fn create_global<R: Resource>(handle: &Handle) -> GlobalId {
    let iface = R::interface();
    handle.create_global::<ProxyState>(
        iface,
        iface.version,
        Arc::new(ProxyGlobal {
            iface_name: iface.name,
        }),
    )
}

/// The accept-and-dispatch loop.
///
/// Polls two sources: the listening socket (a new application connecting) and the backend's own poll fd
/// (a connected client has pending requests). On a connection, insert the client into the backend; on
/// backend readiness, dispatch all pending client requests and flush any queued events back. Runs until an
/// unrecoverable error.
///
/// `EINTR` from `poll` is retried rather than surfaced — a signal is not a failure of the loop.
fn serve(
    backend: &mut Backend<ProxyState>,
    listener: &UnixListener,
    state: &mut ProxyState,
) -> anyhow::Result<()> {
    use std::os::fd::{AsFd, AsRawFd};
    loop {
        // Two fds to watch: index 0 the listener, index 1 the backend. `poll` fills `revents` in place.
        let mut fds = [
            libc::pollfd {
                fd: listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: backend.poll_fd().as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid, initialized array of 2 pollfds for the duration of the call; a negative
        // timeout blocks until an fd is ready. `poll` only writes the `revents` fields.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            // A signal interrupted the wait; that is expected, not a failure. Re-poll.
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        // A new application is connecting: accept it and register it with the backend.
        if fds[0].revents & libc::POLLIN != 0 {
            accept_client(backend, listener)?;
        }
        // A connected client has pending requests (or the backend needs servicing): drain and flush.
        if fds[1].revents & libc::POLLIN != 0 {
            backend.dispatch_all_clients(state)?;
            backend.flush(None)?;
        }
    }
}

/// Accept one pending connection on `listener` and hand its stream to the backend as a new client.
///
/// The stream is registered with a fresh [`ProxyClientData`]; from then on the backend delivers that
/// client's requests to the objects' [`ObjectData::request`]. After registering, flush so the client
/// immediately sees the initial protocol state (e.g. the display's globals).
fn accept_client(
    backend: &mut Backend<ProxyState>,
    listener: &UnixListener,
) -> anyhow::Result<()> {
    // `poll` reported the listener readable, so this accept does not block.
    let (stream, _addr): (UnixStream, _) = listener.accept()?;
    let mut handle = backend.handle();
    let client_id = handle.insert_client(stream, Arc::new(ProxyClientData))?;
    wp_log(&format!("application connected: client {:?}", client_id));
    // Push any pending events (the registry/globals) so the client can proceed without first sending.
    backend.flush(None)?;
    Ok(())
}

/// Emit a bring-up diagnostic line to stderr, gated on `RAYLAND_WP_LOG` so normal runs stay silent.
///
/// This is scaffolding for the globals+connect sub-step — it lets a human confirm which globals vkcube
/// binds and which requests arrive. It follows the repository's env-gated-stderr convention rather than a
/// logging framework; a later sub-step routes real events over the link instead.
fn wp_log(msg: &str) {
    // Only speak when explicitly asked, so the proxy is quiet in production and loud during bring-up.
    if std::env::var_os("RAYLAND_WP_LOG").is_some() {
        eprintln!("[wp-proxy] {msg}");
    }
}
