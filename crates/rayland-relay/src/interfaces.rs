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

/// Whether an application obtains this interface by **binding a global**, or by a **request on an
/// object it already holds**.
///
/// This distinction has to live in the table. Without it, C needs a second hand-maintained list of
/// which names are globals — and a second list is a second thing to drift, which is the exact defect
/// this module exists to end. The first draft of C's advertisement did keep such a list, and adding
/// `wl_output` to the table while forgetting the list made C silently not advertise it while every
/// test stayed green. Found while executing the plan, one interface later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Advertised in the registry; the application binds it by name.
    Global,
    /// Created by a request on a parent object (`wl_compositor.create_surface`,
    /// `zxdg_output_manager_v1.get_xdg_output`, …). Never advertised, but S must still be able to
    /// name it to replay the request that creates it.
    Child,
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
    /// See [`Kind`]. Decides whether C advertises this interface at all.
    pub kind: Kind,
}

/// Every interface WP0 supports, in one place.
///
/// **This table describes the state of the code, not an aspiration.** Adding an entry here without
/// adding the matching descriptor on both sides makes the consistency tests fail, which is the
/// entire point: the table and the two maps move together or the build goes red.
pub const SUPPORTED: &[InterfaceSpec] = &[
    // --- Globals the application binds -------------------------------------------------------
    InterfaceSpec { name: "wl_compositor", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Global },
    InterfaceSpec { name: "xdg_wm_base", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Global },
    InterfaceSpec { name: "wl_seat", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Global },
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
        kind: Kind::Global,
    },
    // v1 deliberately: v2 adds only `wl_shm.release`, which nothing here needs, and a client binds
    // the minimum of what it wants and what is offered.
    InterfaceSpec { name: "wl_shm", max_version: 1, fds: FdPolicy::Substituted("ShmPool"), kind: Kind::Global },
    // Scale, geometry, mode and refresh — of S's monitor, which is the display the application is
    // actually on, so relaying S's real values is correct rather than a leak. Without it a toolkit
    // assumes scale 1 and renders the wrong size on a HiDPI screen. Highest-impact entry in the gap.
    InterfaceSpec { name: "wl_output", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Global },
    // Adds logical (compositor-space) geometry to `wl_output`; paired with it for that reason.
    InterfaceSpec {
        name: "zxdg_output_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Negotiates server- versus client-side decorations. Without it winit falls back to drawing its
    // own, or to none, and the window looks unlike every other window on S's desktop.
    InterfaceSpec {
        name: "zxdg_decoration_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Lets a client NAME a cursor ("default", "text", ...) instead of supplying pixels. This is the
    // interface behind the cursor that never appeared in the 2026-09-01 solarsim acceptance run:
    // with no way to name a shape, and no shm cursor path, the pointer had nothing to show.
    InterfaceSpec {
        name: "wp_cursor_shape_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Source-crop and destination-size for a surface: half of fractional scaling.
    InterfaceSpec {
        name: "wp_viewporter",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // The other half: tells the client the fractional scale the compositor wants. Paired with
    // `wp_viewporter` for the same reason `wl_output` and `zxdg_output_manager_v1` are paired --
    // knowing the scale is useless without the means to render at it.
    InterfaceSpec {
        name: "wp_fractional_scale_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Presentation timestamps from the compositor. Beyond closing the gap this is of independent
    // value to the project: it is the only COMPOSITOR-SIDE measurement of when a frame was actually
    // shown, and therefore an outside check on Rayland's own frame-time numbers, which have so far
    // only ever been measured by Rayland.
    InterfaceSpec {
        name: "wp_presentation",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Subsurfaces: a video pane or GL canvas inside application chrome.
    InterfaceSpec {
        name: "wl_subcompositor",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Request or transfer focus -- "open this window and raise it".
    InterfaceSpec {
        name: "xdg_activation_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Pointer lock and confinement, needed by anything with a 3D camera.
    InterfaceSpec {
        name: "zwp_pointer_constraints_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Unaccelerated pointer deltas, the companion to a locked pointer.
    InterfaceSpec {
        name: "zwp_relative_pointer_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Input methods. Without it there is no CJK or emoji entry at all.
    InterfaceSpec {
        name: "zwp_text_input_manager_v3",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // Lets a client destroy a wl_registry; libwayland's own globals helper uses it.
    InterfaceSpec {
        name: "wl_fixes",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Global,
    },
    // --- Objects created from those globals, which S must also be able to name ---------------
    // From wl_subcompositor.get_subsurface.
    InterfaceSpec {
        name: "wl_subsurface",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From xdg_activation_v1.get_activation_token.
    InterfaceSpec {
        name: "xdg_activation_token_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From zwp_pointer_constraints_v1.lock_pointer.
    InterfaceSpec {
        name: "zwp_locked_pointer_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From zwp_pointer_constraints_v1.confine_pointer.
    InterfaceSpec {
        name: "zwp_confined_pointer_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From zwp_relative_pointer_manager_v1.get_relative_pointer.
    InterfaceSpec {
        name: "zwp_relative_pointer_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From zwp_text_input_manager_v3.get_text_input.
    InterfaceSpec {
        name: "zwp_text_input_v3",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },

    // From `wp_presentation.feedback`, one per presented frame.
    InterfaceSpec {
        name: "wp_presentation_feedback",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From `wp_viewporter.get_viewport`.
    InterfaceSpec {
        name: "wp_viewport",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From `wp_fractional_scale_manager_v1.get_fractional_scale`.
    InterfaceSpec {
        name: "wp_fractional_scale_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From `wp_cursor_shape_manager_v1.get_pointer`.
    InterfaceSpec {
        name: "wp_cursor_shape_device_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From `zxdg_decoration_manager_v1.get_toplevel_decoration`.
    InterfaceSpec {
        name: "zxdg_toplevel_decoration_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Transparent,
        kind: Kind::Child,
    },
    // From `zxdg_output_manager_v1.get_xdg_output`. Not a global: never bound, only created.
    InterfaceSpec { name: "zxdg_output_v1", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec { name: "wl_surface", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec { name: "wl_region", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec { name: "wl_callback", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec { name: "wl_buffer", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec {
        name: "wl_shm_pool",
        max_version: u32::MAX,
        fds: FdPolicy::Substituted("ShmPool"),
        kind: Kind::Child,
    },
    InterfaceSpec { name: "xdg_surface", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec { name: "xdg_toplevel", max_version: u32::MAX, fds: FdPolicy::Transparent, kind: Kind::Child },
    InterfaceSpec {
        name: "zwp_linux_buffer_params_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Substituted("BufferToken"),
        kind: Kind::Child,
    },
    // --- Known, and deliberately NOT advertised ------------------------------------------------
    // These are the reason `FdPolicy::Refused` exists. Withholding an optional global is CORRECT
    // Wayland behaviour -- applications cope -- so the defect was never the absence. It was that an
    // absence and an oversight looked identical. Both are now stated at startup, with why.
    InterfaceSpec {
        name: "wp_linux_drm_syncobj_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Refused(
            "carries a DRM syncobj fd; cross-machine explicit GPU sync is undesigned",
        ),
        kind: Kind::Global,
    },
    InterfaceSpec {
        name: "wl_data_device_manager",
        max_version: u32::MAX,
        fds: FdPolicy::Refused("clipboard/DnD transfer over app-created fds; its own phase"),
        kind: Kind::Global,
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
/// The entries C should advertise as registry globals: [`Kind::Global`] and not
/// [`FdPolicy::Refused`].
///
/// This is the whole global list, derived from the one table. C has no second list to keep in step.
pub fn advertised_globals() -> impl Iterator<Item = &'static InterfaceSpec> {
    advertised().filter(|s| s.kind == Kind::Global)
}

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

    /// The five pre-existing globals are tagged `Global`, and the objects created from them are
    /// tagged `Child`. A mis-tag is invisible at runtime — a `Child`-tagged global is simply never
    /// advertised — so it is pinned here.
    #[test]
    fn the_global_child_split_is_right() {
        let globals: Vec<_> = advertised_globals().map(|s| s.name).collect();
        for name in ["wl_compositor", "xdg_wm_base", "zwp_linux_dmabuf_v1", "wl_seat", "wl_shm"] {
            assert!(globals.contains(&name), "{name} must be advertised as a global");
        }
        for name in ["wl_surface", "wl_buffer", "wl_shm_pool", "xdg_surface", "xdg_toplevel"] {
            assert!(!globals.contains(&name), "{name} is created by a request, not bound");
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
