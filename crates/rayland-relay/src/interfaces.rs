//! The single declaration of which Wayland interfaces the WP0 proxy supports.
//!
//! # Why this exists
//!
//! Before this module the supported set was written down **twice**: `rayland-c` advertised globals
//! with `create_global::<WlCompositor>()` over `wayland-server`'s descriptors, and `rayland-s`
//! resolved binds with a hand-written `match` over `wayland-client`'s. Nothing tied the two
//! together, and on 2026-09-01 they drifted: `wl_shm` was added to C and forgotten in S. C forwarded
//! the application's `create_pool` perfectly, S logged `no linked descriptor for wl_shm; bind
//! skipped`, and because the application's GPU frames go through dma-buf the only symptom was a
//! cursor that never appeared. A human noticing a missing cursor was the entire detection mechanism.
//!
//! This module is the fix. It names each interface once; C drives its registry from it, and S is
//! tested against it in both directions. Forgetting one side is now a failing test, not a silence.
//!
//! # Why it holds names rather than descriptors
//!
//! It cannot hold the interface descriptors themselves. `WlCompositor` in `wayland-server` and
//! `WlCompositor` in `wayland-client` are different Rust types with different `&'static Interface`
//! values, and this crate must depend on neither — it is pure data, shared by a machine that may
//! never link a GPU stack. Sharing the *names*, version caps and fd policy is the most that is true,
//! and it is enough to make the two maps testable against each other.

/// What an interface does about file descriptors.
///
/// This project's founding constraint is that **a file descriptor cannot cross a network**. WP0
/// already answers that three times, and those answers form a family: `BufferToken` sends a *name*
/// for a resource S already holds, `KeymapContent` sends the *contents*, and `ShmPool` sends a
/// *size*. Every interface must declare which case it is, so that "this one carries an fd and we
/// have not designed a substitution" is a recorded decision rather than an oversight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdPolicy {
    /// Carries no file descriptors; requests and events relay unchanged.
    Transparent,
    /// Carries a descriptor with a designed replacement. The payload names the substitution, for
    /// diagnostics — e.g. `"BufferToken"`, `"KeymapContent"`, `"ShmPool"`.
    Substituted(&'static str),
    /// Carries a descriptor with **no** designed substitution, so the global is deliberately not
    /// advertised. The payload is the reason, which C prints at startup: a withheld global must be
    /// a statement, never a silence. Not advertising an optional global is correct Wayland
    /// behaviour — applications are built to cope — so this is a scope decision, not a defect.
    Refused(&'static str),
}

/// One interface WP0 knows about.
#[derive(Debug, Clone, Copy)]
pub struct InterfaceSpec {
    /// The wire name, exactly as it appears in `wl_registry.global` — e.g. `"wl_output"`.
    pub name: &'static str,
    /// The highest version WP0 will advertise, independent of what either side's descriptor
    /// supports. A cap belongs here, as data, because this project has hit version-inheritance
    /// bugs three times and a cap that lives in code is a cap somebody has to remember.
    pub max_version: u32,
    /// See [`FdPolicy`].
    pub fds: FdPolicy,
}

/// Every interface WP0 supports, in one place.
///
/// **This table describes the state of the code, not an aspiration.** Adding an entry here without
/// adding the matching descriptor on both sides makes the consistency tests fail, which is the
/// entire point: the table and the two maps move together or the build goes red.
pub const SUPPORTED: &[InterfaceSpec] = &[
    // --- Globals the application binds -------------------------------------------------------
    InterfaceSpec { name: "wl_compositor", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "xdg_wm_base", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_seat", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // **Capped at v3 on purpose, and the cap is load-bearing (WP0 Task 4.4).**
    //
    // The interface descriptor supports higher versions, but Mesa's Venus WSI opts into the **v4
    // feedback** path (`get_default_feedback`) whenever the bound version is >= 4, and that path
    // delivers its supported formats through a `format_table` **file descriptor** the client
    // `mmap`s (`wsi_common_wayland.c:917-928`). A file descriptor cannot cross a network. At v3
    // Mesa falls back to the plain `modifier` event (`:830-852`) -- three integers and no fd, a
    // complete path in this Mesa. So WP0 advertises exactly v3, forcing the fd-free path, and
    // answers the format query itself (`rayland_c::wayland_proxy::advertise_dmabuf_formats`).
    InterfaceSpec {
        name: "zwp_linux_dmabuf_v1",
        max_version: 3,
        fds: FdPolicy::Substituted("BufferToken"),
    },
    // v1 deliberately: v2 adds only `wl_shm.release`, which nothing here needs, and a client binds
    // the minimum of what it wants and what is offered.
    InterfaceSpec { name: "wl_shm", max_version: 1, fds: FdPolicy::Substituted("ShmPool") },
    // --- Objects created from those globals, which S must also be able to name ---------------
    InterfaceSpec { name: "wl_surface", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_region", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_callback", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_buffer", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec {
        name: "wl_shm_pool",
        max_version: u32::MAX,
        fds: FdPolicy::Substituted("ShmPool"),
    },
    InterfaceSpec { name: "xdg_surface", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "xdg_toplevel", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec {
        name: "zwp_linux_buffer_params_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Substituted("BufferToken"),
    },
];

/// The entries `rayland-c` should advertise in the application's registry: everything that is not
/// [`FdPolicy::Refused`].
///
/// Note this yields child-object interfaces too (`wl_surface` and friends). Advertising is done by
/// the caller only for entries that are *globals*; this iterator is the policy filter, not the
/// global list. The caller pairs it with its own descriptor map, and an entry with no server-side
/// descriptor is simply not advertised — which the consistency tests then catch.
pub fn advertised() -> impl Iterator<Item = &'static InterfaceSpec> {
    SUPPORTED.iter().filter(|s| !matches!(s.fds, FdPolicy::Refused(_)))
}

/// Look one interface up by its wire name.
///
/// Returns `None` for an interface WP0 has never heard of, which is different from one it has
/// deliberately refused — a refused interface is present with [`FdPolicy::Refused`]. Callers that
/// report to a human must distinguish the two, because "we decided against this" and "we have never
/// considered this" are different answers.
pub fn spec_for(name: &str) -> Option<&'static InterfaceSpec> {
    SUPPORTED.iter().find(|s| s.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every entry must be uniquely named. A duplicate would make `spec_for` return
    /// whichever came first, so C and S could silently disagree about a version cap.
    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in SUPPORTED {
            assert!(seen.insert(spec.name), "duplicate interface entry: {}", spec.name);
        }
    }

    /// `advertised()` is exactly the non-refused entries. This is the function C drives its
    /// registry from, so an off-by-one here is an interface the application cannot see.
    #[test]
    fn advertised_excludes_only_refused() {
        let advertised: Vec<_> = advertised().map(|s| s.name).collect();
        for spec in SUPPORTED {
            let is_refused = matches!(spec.fds, FdPolicy::Refused(_));
            assert_eq!(
                !is_refused,
                advertised.contains(&spec.name),
                "{} refused={is_refused} but advertised={}",
                spec.name,
                advertised.contains(&spec.name)
            );
        }
    }

    /// A `Refused` entry must carry a non-empty reason: the whole point of the variant is that
    /// a withheld global is a *statement*, not a silence (spec §5).
    #[test]
    fn refused_entries_state_a_reason() {
        for spec in SUPPORTED {
            if let FdPolicy::Refused(reason) = spec.fds {
                assert!(!reason.is_empty(), "{} is Refused with no reason", spec.name);
            }
        }
    }

    /// The five globals WP0 served before this phase must still be advertised. This is the
    /// regression guard for Task 3, which replaces five hardcoded calls with a table walk.
    #[test]
    fn the_pre_existing_five_globals_are_still_advertised() {
        let advertised: Vec<_> = advertised().map(|s| s.name).collect();
        for name in ["wl_compositor", "xdg_wm_base", "zwp_linux_dmabuf_v1", "wl_seat", "wl_shm"] {
            assert!(advertised.contains(&name), "{name} is no longer advertised");
        }
    }

    /// `zwp_linux_dmabuf_v1` is capped at v3 so Mesa takes the fd-free format path. Losing that
    /// cap is a silent behaviour change, so it is pinned here rather than left to a comment.
    #[test]
    fn dmabuf_stays_capped_at_v3() {
        assert_eq!(spec_for("zwp_linux_dmabuf_v1").expect("dmabuf is supported").max_version, 3);
    }

    /// `wl_shm` is advertised at v1 deliberately (v2 adds only `wl_shm.release`, which nothing
    /// here needs).
    #[test]
    fn shm_stays_at_v1() {
        assert_eq!(spec_for("wl_shm").expect("shm is supported").max_version, 1);
    }
}
