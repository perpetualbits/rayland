# (c)4a Wayland Protocol Breadth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve the Wayland globals a real toolkit application actually binds, and make it structurally impossible for the two sides of the proxy to disagree about which those are.

**Architecture:** One shared const table in `rayland-relay` names every interface WP0 supports, with a version cap and a file-descriptor policy. `rayland-c` advertises from it; `rayland-s` is tested against it in both directions. Interfaces are then added to that one table, in priority order, each with a two-sided test.

**Tech Stack:** Rust 2024, `wayland-server` (C side), `wayland-client` (S side), `wayland-protocols` 0.32.12 (features `client`, `server`, `staging`, `unstable` — **already enabled in the workspace; no dependency changes are needed by this plan**).

**Spec:** [`docs/superpowers/specs/2026-09-02-c4-protocol-breadth-design.md`](../specs/2026-09-02-c4-protocol-breadth-design.md)

## Global Constraints

- **A doc-comment block (`///` or `//!`) on every function, type, trait and module.** Repository rule, `CLAUDE.md`.
- **An intent comment on every non-trivial line** — the *why* or the domain meaning, never a restatement of syntax.
- **Code and comments must always agree.** A stale comment is a bug, fixed in the same edit.
- **`rayland-relay` must stay pure data**: no GPU code, no sockets, no async runtime. This plan adds only a const table and tests to it — no new dependencies.
- **`rayland-c` must never link a GPU stack.** `crates/rayland-c/tests/no_gpu_linkage.rs` guards this; it must still pass after every task.
- **The acceptance criterion is never a list of supported interfaces** (spec §8). A test that enumerates what it expects cannot find the entry you forgot.
- **Every duration figure requires `PROFILE=release`.** No timing claims are made by this plan.
- Commit after every task. Push is the owner's call.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/rayland-relay/src/interfaces.rs` | **Create.** The single declaration: `FdPolicy`, `InterfaceSpec`, `SUPPORTED`. Pure data. |
| `crates/rayland-relay/src/lib.rs` | **Modify.** Add `pub mod interfaces;`. |
| `crates/rayland-s/src/wayland_client.rs` | **Modify.** Extend `interface_by_name`; add the two-sided consistency tests. |
| `crates/rayland-c/src/wayland_proxy.rs` | **Modify.** Advertise from `SUPPORTED` via a new server-side `interface_by_name`; log the registry decision at startup. |
| `scripts/wp0-bind-gap.sh` | **Create.** The bind-gap report (spec §6.2). |

---

### Task 1: The shared declaration

**Files:**
- Create: `crates/rayland-relay/src/interfaces.rs`
- Modify: `crates/rayland-relay/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `rayland_relay::interfaces::{FdPolicy, InterfaceSpec, SUPPORTED, advertised, spec_for}`.
  - `pub enum FdPolicy { Transparent, Substituted(&'static str), Refused(&'static str) }`
  - `pub struct InterfaceSpec { pub name: &'static str, pub max_version: u32, pub fds: FdPolicy }`
  - `pub const SUPPORTED: &[InterfaceSpec]`
  - `pub fn advertised() -> impl Iterator<Item = &'static InterfaceSpec>` — entries C should advertise (everything not `Refused`).
  - `pub fn spec_for(name: &str) -> Option<&'static InterfaceSpec>`

- [ ] **Step 1: Write the failing test**

Create `crates/rayland-relay/src/interfaces.rs` containing **only** this test module for now:

```rust
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
```

Add to `crates/rayland-relay/src/lib.rs`, after the existing `pub mod` lines:

```rust
/// The one place both sides of the WP0 proxy agree on which Wayland interfaces exist.
pub mod interfaces;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-relay --lib interfaces`
Expected: FAIL to compile — `cannot find type FdPolicy`, `cannot find value SUPPORTED`.

- [ ] **Step 3: Write minimal implementation**

Prepend to `crates/rayland-relay/src/interfaces.rs`, above the test module:

```rust
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
//! tested against it in both directions (see `wayland_client.rs`'s consistency tests). Forgetting
//! one side is now a failing test rather than a silence.
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
    // Capped at v3 so Mesa takes the fd-free format path; v4+ hands out a format *table* in a
    // descriptor, which is exactly the thing that cannot cross a network.
    InterfaceSpec { name: "zwp_linux_dmabuf_v1", max_version: 3, fds: FdPolicy::Substituted("BufferToken") },
    // v1 deliberately: v2 adds only `wl_shm.release`, which nothing here needs, and a client binds
    // the minimum of what it wants and what is offered.
    InterfaceSpec { name: "wl_shm", max_version: 1, fds: FdPolicy::Substituted("ShmPool") },
    // --- Objects created from those globals, which S must also be able to name ---------------
    InterfaceSpec { name: "wl_surface", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_region", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_callback", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_buffer", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_shm_pool", max_version: u32::MAX, fds: FdPolicy::Substituted("ShmPool") },
    InterfaceSpec { name: "xdg_surface", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "xdg_toplevel", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "zwp_linux_buffer_params_v1", max_version: u32::MAX, fds: FdPolicy::Substituted("BufferToken") },
];

/// The entries `rayland-c` should advertise in the application's registry: everything that is not
/// [`FdPolicy::Refused`].
///
/// Note this yields child-object interfaces too (`wl_surface` and friends). Advertising is done by
/// the caller only for entries that are *globals*; this iterator is the policy filter, not the
/// global list. Task 3's caller pairs it with its own descriptor map, and an entry with no
/// server-side descriptor is simply not advertised — which the consistency tests then catch.
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rayland-relay --lib interfaces`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-relay/src/interfaces.rs crates/rayland-relay/src/lib.rs
git commit -m "relay: one shared declaration of the WP0 interface set

The supported set was written down twice -- C's create_global calls over
wayland-server descriptors, S's interface_by_name over wayland-client ones --
and that drift IS the wl_shm bug: added to C, forgotten in S, detected by a
human noticing a missing cursor. This is the single table both sides will be
held to. It describes today's behaviour exactly; nothing changes yet."
```

---

### Task 2: S is held to the table

**Files:**
- Modify: `crates/rayland-s/src/wayland_client.rs` (the `tests` module at the end, and `Cargo.toml` if `rayland-relay` is not already a dev-dependency — it is a normal dependency already, so no change is expected)

**Interfaces:**
- Consumes: `rayland_relay::interfaces::{SUPPORTED, spec_for}` from Task 1.
- Produces: nothing new in the public API. Produces the *invariant* every later task relies on.

- [ ] **Step 1: Write the failing test**

Replace the existing `interface_registry_maps_the_wp0_interfaces` test in `crates/rayland-s/src/wayland_client.rs` with these two. Delete the old one — it enumerates the names it expects, so it can only catch a name someone remembered to add in two places at once, which is exactly how `wl_shm` got through.

```rust
    /// Every interface the shared table names must resolve here, or S cannot replay a bind of it.
    ///
    /// This replaces a test that listed the eleven names it expected and asserted they resolve.
    /// That test passed while `wl_shm` was missing, because the name was absent from both the code
    /// and the test. This one compares against a list maintained for a *different* purpose — C's
    /// advertisement — so forgetting one side is a failure rather than a silence.
    #[test]
    fn every_supported_interface_resolves() {
        for spec in rayland_relay::interfaces::SUPPORTED {
            assert!(
                interface_by_name(spec.name).is_some(),
                "`{}` is in rayland_relay::interfaces::SUPPORTED but S has no linked descriptor \
                 for it, so a bind would be dropped mid-session",
                spec.name
            );
        }
    }

    /// And nothing resolves here that the table does not name. A descriptor S can build but C never
    /// advertises is dead code at best; at worst it is an interface someone added on one side only,
    /// which is the same drift in the other direction.
    #[test]
    fn nothing_resolves_that_the_table_does_not_name() {
        for name in KNOWN_INTERFACE_NAMES {
            assert!(
                rayland_relay::interfaces::spec_for(name).is_some(),
                "S resolves `{name}` but it is not in rayland_relay::interfaces::SUPPORTED"
            );
        }
    }
```

Add, immediately above `fn interface_by_name`, the list the second test walks — kept next to the `match` it mirrors so the two are edited together:

```rust
/// Every name [`interface_by_name`] answers to.
///
/// Rust cannot enumerate a `match`'s arms, so this list exists to let the consistency test walk
/// them. **Keep it in step with the `match` below**: a name here that the `match` lacks fails
/// `every_supported_interface_resolves` if the table names it, and a `match` arm missing here is
/// caught the first time anything binds it. Both are better than the silence this replaces.
const KNOWN_INTERFACE_NAMES: &[&str] = &[
    "wl_compositor",
    "wl_surface",
    "wl_region",
    "wl_callback",
    "wl_buffer",
    "wl_shm",
    "wl_shm_pool",
    "wl_seat",
    "xdg_wm_base",
    "xdg_surface",
    "xdg_toplevel",
    "zwp_linux_dmabuf_v1",
    "zwp_linux_buffer_params_v1",
];
```

- [ ] **Step 2: Run test to verify it fails**

First prove the test has teeth, because a consistency test that cannot fail is worse than none.

Temporarily delete the `"wl_shm" => WlShm::interface(),` arm from `interface_by_name` and delete `"wl_shm"` from `KNOWN_INTERFACE_NAMES`.

Run: `cargo test -p rayland-s --lib wayland_client`
Expected: FAIL — `` `wl_shm` is in rayland_relay::interfaces::SUPPORTED but S has no linked descriptor for it ``. **This is the 2026-09-01 bug, reproduced as a test failure.**

Restore both. Then mutate the other direction: add `InterfaceSpec { name: "wl_output", max_version: u32::MAX, fds: FdPolicy::Transparent },` to `SUPPORTED` in `crates/rayland-relay/src/interfaces.rs`.

Run: `cargo test -p rayland-s --lib wayland_client`
Expected: FAIL — `` `wl_output` is in ... SUPPORTED but S has no linked descriptor ``.

Remove that entry again.

- [ ] **Step 3: Write minimal implementation**

None. Both tests pass against the unmodified code, which is the point: Task 1's table was written to describe today exactly.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rayland-s --lib wayland_client`
Expected: PASS.

Run: `cargo test --workspace`
Expected: PASS, no regressions.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-s/src/wayland_client.rs
git commit -m "s: hold the interface map to the shared table, in both directions

Deletes the old registry test, which enumerated the eleven names it expected
and so passed while wl_shm was missing. The replacements compare S's map
against rayland-relay's SUPPORTED table both ways. Verified by mutation: with
the wl_shm arm removed the 2026-09-01 bug reproduces as a test failure, and an
entry added to the table with no descriptor fails the other direction."
```

---

### Task 3: C advertises from the table, and says so

**Files:**
- Modify: `crates/rayland-c/src/wayland_proxy.rs` (the `use` block near line 238; `create_global`; the advertisement block near line 1266)

**Interfaces:**
- Consumes: `rayland_relay::interfaces::{FdPolicy, InterfaceSpec, advertised}`.
- Produces: `fn server_interface_by_name(name: &str) -> Option<&'static wayland_server::backend::protocol::Interface>` — C's name→descriptor map, mirroring S's.

- [ ] **Step 1: Write the failing test**

Add to `crates/rayland-c/src/wayland_proxy.rs`'s test module:

```rust
    /// C must be able to name every global the shared table says to advertise.
    ///
    /// The mirror of S's `every_supported_interface_resolves`. Child-object interfaces
    /// (`wl_surface` and friends) are not globals and are not advertised, so they are exempt —
    /// C only ever creates globals.
    #[test]
    fn every_advertised_global_has_a_server_descriptor() {
        for spec in rayland_relay::interfaces::advertised() {
            if !GLOBAL_INTERFACE_NAMES.contains(&spec.name) {
                continue; // a child object, created by a request rather than bound
            }
            assert!(
                server_interface_by_name(spec.name).is_some(),
                "`{}` is advertised by the shared table but C has no server descriptor for it",
                spec.name
            );
        }
    }

    /// The five globals WP0 served before this phase are still advertised, at the same versions.
    /// Task 3 replaces five hardcoded `create_global` calls with a table walk; this is the guard
    /// that the replacement is behaviour-preserving.
    #[test]
    fn the_advertised_globals_are_unchanged_by_the_table_walk() {
        let mut got: Vec<(&str, u32)> = rayland_relay::interfaces::advertised()
            .filter(|s| GLOBAL_INTERFACE_NAMES.contains(&s.name))
            .map(|s| {
                let iface = server_interface_by_name(s.name).expect("descriptor exists");
                (s.name, s.max_version.min(iface.version))
            })
            .collect();
        got.sort();
        let mut want = vec![
            ("wl_compositor", wayland_server::protocol::wl_compositor::WlCompositor::interface().version),
            ("wl_seat", wayland_server::protocol::wl_seat::WlSeat::interface().version),
            ("wl_shm", 1),
            ("xdg_wm_base", wayland_protocols::xdg::shell::server::xdg_wm_base::XdgWmBase::interface().version),
            ("zwp_linux_dmabuf_v1", 3),
        ];
        want.sort();
        assert_eq!(got, want);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rayland-c --lib wayland_proxy`
Expected: FAIL to compile — `cannot find function server_interface_by_name`, `cannot find value GLOBAL_INTERFACE_NAMES`.

- [ ] **Step 3: Write minimal implementation**

Add near the existing descriptor `use` block (around line 238):

```rust
/// Every interface in the shared table that is a **global** — something the application binds from
/// the registry, as opposed to a child object created by a request. Only these are advertised.
///
/// Kept beside [`server_interface_by_name`] so the two are edited together, and walked by the
/// consistency test so a name here without a descriptor is a build failure.
const GLOBAL_INTERFACE_NAMES: &[&str] =
    &["wl_compositor", "xdg_wm_base", "zwp_linux_dmabuf_v1", "wl_seat", "wl_shm"];

/// Map a wire interface name to the `wayland-server` descriptor `create_global` needs.
///
/// The mirror of `rayland-s`'s `interface_by_name`, and it must be a separate function for a reason
/// worth stating: `WlCompositor` in `wayland-server` and `WlCompositor` in `wayland-client` are
/// different Rust types with different `&'static Interface` values, so the two sides cannot share
/// one map. What they share is `rayland_relay::interfaces::SUPPORTED`, and the tests hold both maps
/// to it.
fn server_interface_by_name(name: &str) -> Option<&'static wayland_server::backend::protocol::Interface> {
    Some(match name {
        "wl_compositor" => WlCompositor::interface(),
        "xdg_wm_base" => XdgWmBase::interface(),
        "zwp_linux_dmabuf_v1" => ZwpLinuxDmabufV1::interface(),
        "wl_seat" => WlSeat::interface(),
        "wl_shm" => WlShm::interface(),
        _ => return None,
    })
}
```

Replace the five hardcoded `create_global::<…>` calls (around line 1273) with the table walk, and add the startup report:

```rust
    // Advertise from the shared table rather than from five hardcoded calls, so that C and S cannot
    // disagree about the supported set (see `rayland_relay::interfaces`). Child-object interfaces in
    // the table are skipped: they are created by requests, not bound from the registry.
    for spec in rayland_relay::interfaces::advertised() {
        if !GLOBAL_INTERFACE_NAMES.contains(&spec.name) {
            continue;
        }
        let Some(iface) = server_interface_by_name(spec.name) else {
            // Unreachable in a tested build — `every_advertised_global_has_a_server_descriptor`
            // fails first — but reported rather than ignored, because silence here is the exact
            // failure mode this phase exists to end.
            wp_log(&format!(
                "registry: `{}` is in the shared table but C has no descriptor; NOT advertised",
                spec.name
            ));
            continue;
        };
        let version = spec.max_version.min(iface.version);
        handle.create_global::<ProxyState>(iface, version, GlobalData);
        wp_log(&format!("registry: advertising {} v{version}", spec.name));
    }
    // State what was deliberately withheld, and why. A withheld global is correct Wayland behaviour
    // — applications cope with an absent optional global — but an *unrecorded* absence is
    // indistinguishable from an oversight, which is how fourteen interfaces went missing unnoticed.
    for spec in rayland_relay::interfaces::SUPPORTED {
        if let rayland_relay::interfaces::FdPolicy::Refused(reason) = spec.fds {
            wp_log(&format!("registry: NOT advertising {} — {reason}", spec.name));
        }
    }
```

Delete the now-unused `create_global` helper function and its doc comment if nothing else calls it; if the compiler reports it unused, remove it rather than leaving it dead.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rayland-c --lib wayland_proxy`
Expected: PASS.

Run: `cargo test -p rayland-c --test no_gpu_linkage`
Expected: PASS — `rayland-relay` is pure data, so C still links no GPU stack.

- [ ] **Step 5: Verify the proxy still serves a real application**

Run: `PROFILE=release scripts/wp0-soak.sh` with `TRIES=5`.
Expected: 5/5 clean, and the log contains five `registry: advertising …` lines.

- [ ] **Step 6: Commit**

```bash
git add crates/rayland-c/src/wayland_proxy.rs
git commit -m "c: advertise from the shared table, and report the registry decision

Replaces five hardcoded create_global calls with a walk of
rayland-relay's SUPPORTED, plus a server-side name->descriptor map held to
the same table by test. C now logs what it advertises and what it withheld
with the reason, so a deliberate omission is distinguishable from an
oversight -- which it was not before."
```

---

### Task 4: The bind-gap report

**Files:**
- Create: `scripts/wp0-bind-gap.sh`

**Interfaces:**
- Consumes: nothing in Rust. Reads `WAYLAND_DEBUG` output.
- Produces: a script taking `APP=<path>` and printing the interfaces an application binds against a real compositor that WP0 does not offer.

- [ ] **Step 1: Write the script**

```bash
#!/usr/bin/env bash
#
# THE BIND-GAP REPORT: what does this application ask for that WP0 does not offer?
# =============================================================================================
#
# Run an application against a real compositor, capture every `wl_registry.bind`, and diff that
# against the globals WP0 advertises. The output is the honest answer to "what is this program
# missing when we run it", for ANY program — including ones nobody has tried.
#
# Why this rather than a list of supported interfaces: the repository already learned that a test
# enumerating the things it supports cannot find the one you forgot. S's registry test asserted all
# eleven listed names resolve, and passed while `wl_shm` was missing. Only a real application asking
# for something can tell you what is absent.
#
# TWO TRAPS, both paid for on 2026-09-02:
#
#   1. RUN AGAINST A FULL COMPOSITOR, NEVER HEADLESS WESTON. Headless weston advertises no
#      `wl_seat`, so every sweep this project ran was structurally blind to an entire class of
#      interface. Use the live session.
#   2. THE TRACE FORMAT VARIES. libwayland prints `wl_registry@2` in some builds and
#      `wl_registry#2` in others; both were seen on the same machine on 2026-09-02, and the first
#      attempt at this capture extracted nothing because of it. The pattern below accepts both, and
#      the script FAILS LOUDLY on zero matches rather than reporting an empty gap — an empty diff
#      must mean "nothing missing", never "the parser did not match".
#
# Usage:
#   APP=/path/to/app scripts/wp0-bind-gap.sh
#   APP=~/.cargo/bin/solarsim SECONDS_TO_RUN=8 scripts/wp0-bind-gap.sh
set -u
APP="${APP:?set APP=/path/to/the/application}"
SECONDS_TO_RUN="${SECONDS_TO_RUN:-8}"
OUT="${OUT:-/tmp/wp0-bind-gap}"
mkdir -p "$OUT"

[ -n "${WAYLAND_DISPLAY:-}" ] || { echo "ABORTING: no WAYLAND_DISPLAY; run this from the live session." >&2; exit 1; }
case "${WAYLAND_DISPLAY}" in
  *soak*|*headless*) echo "ABORTING: WAYLAND_DISPLAY=$WAYLAND_DISPLAY looks headless. See trap 1." >&2; exit 1 ;;
esac

echo "### tracing $(basename "$APP") against $WAYLAND_DISPLAY for ${SECONDS_TO_RUN}s ###"
WAYLAND_DEBUG=1 timeout "$SECONDS_TO_RUN" "$APP" >"$OUT/trace.log" 2>&1

# Accept BOTH `@` and `#` object separators. See trap 2.
grep -oE 'wl_registry[#@][0-9]+\.bind\([0-9]+, *"[a-zA-Z_0-9]+", *[0-9]+' "$OUT/trace.log" \
  | sed -E 's/.*"([a-zA-Z_0-9]+)", *([0-9]+)/\1 \2/' | sort -u > "$OUT/bound.txt"

if [ ! -s "$OUT/bound.txt" ]; then
  echo "ABORTING: extracted ZERO binds from $OUT/trace.log." >&2
  echo "  An empty gap must mean 'nothing missing', never 'the parser did not match'." >&2
  echo "  Check the trace format, and whether the application redirects its own stderr" >&2
  echo "  (rt does: crashlog::capture_stderr_if_not_a_tty writes to ~/.cache/rt/stderr.log)." >&2
  exit 1
fi

echo "### this application binds $(wc -l < "$OUT/bound.txt") globals ###"
echo "### what WP0 does not offer ###"
gap=0
while read -r name version; do
  case "$(grep -c "^${name}\$" "$OUT/wp0-offers.txt" 2>/dev/null || echo 0)" in
    0) printf "  MISSING  %-38s (app wants v%s)\n" "$name" "$version"; gap=$((gap+1)) ;;
  esac
done < "$OUT/bound.txt"
[ "$gap" -eq 0 ] && echo "  (none)"
echo "### gap: $gap interface(s) ###"
```

- [ ] **Step 2: Generate the list of what WP0 offers**

The script diffs against `$OUT/wp0-offers.txt`. Produce it from the table itself, so it cannot drift:

```bash
cargo run -q -p rayland-c --bin rayland-c -- --print-globals > /tmp/wp0-bind-gap/wp0-offers.txt
```

That flag does not exist yet. Add it to `crates/rayland-c/src/main.rs`, before any daemon setup:

```rust
    // `--print-globals` exists for `scripts/wp0-bind-gap.sh`: it needs the list of advertised
    // globals, and deriving it from the shared table rather than hardcoding it in the script is
    // what stops the report drifting from the code it reports on.
    if std::env::args().any(|a| a == "--print-globals") {
        for spec in rayland_relay::interfaces::advertised() {
            println!("{}", spec.name);
        }
        return Ok(());
    }
```

- [ ] **Step 3: Prove the report has teeth**

A report that always prints "(none)" would pass silently. Verify it can fail:

```bash
chmod +x scripts/wp0-bind-gap.sh
mkdir -p /tmp/wp0-bind-gap
printf 'wl_compositor\n' > /tmp/wp0-bind-gap/wp0-offers.txt   # a deliberately short offer list
APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh
```

Expected: a **large** gap (~18 interfaces), not "(none)". This proves the diff works before it is ever used to certify success.

Then restore the real list and re-run:

```bash
cargo run -q -p rayland-c --bin rayland-c -- --print-globals > /tmp/wp0-bind-gap/wp0-offers.txt
APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh
```

Expected: gap of **14**, matching spec §2 exactly. If it does not match, the spec's measurement or this parser is wrong — investigate before proceeding, do not adjust the expected number.

- [ ] **Step 4: Commit**

```bash
git add scripts/wp0-bind-gap.sh crates/rayland-c/src/main.rs
git commit -m "wp0: the bind-gap report, and --print-globals to feed it

Answers 'what does this application ask for that WP0 does not offer' for any
application, by diffing a real-compositor trace against the shared table. It
refuses to run against a headless compositor (no wl_seat means blind to a
whole class) and fails loudly on zero parsed binds, because an empty gap must
mean nothing is missing and never that the parser did not match. Verified
against a deliberately short offer list, so the diff is known to be able to
fail before it is used to certify success."
```

---

### Task 5: `wl_output` and `zxdg_output_manager_v1`

The highest-impact pair: without them a toolkit assumes scale 1, and on a HiDPI display the window is visibly the wrong size. They pair naturally — `zxdg_output_manager_v1` exists to add logical geometry to `wl_output`. These describe **S's** monitor, and relaying S's real values is correct: the application is displayed there, so that is the truth it should see.

**Files:**
- Modify: `crates/rayland-relay/src/interfaces.rs`
- Modify: `crates/rayland-c/src/wayland_proxy.rs`
- Modify: `crates/rayland-s/src/wayland_client.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–3.
- Produces: nothing new; two more names resolve on both sides.

- [ ] **Step 1: Add both entries to the shared table**

In `crates/rayland-relay/src/interfaces.rs`, after the `wl_shm` entry:

```rust
    // Scale, geometry, mode and refresh — of S's monitor, which is the display the application is
    // actually on. Without it a toolkit assumes scale 1 and renders the wrong size on HiDPI.
    InterfaceSpec { name: "wl_output", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Adds logical (compositor-space) geometry to `wl_output`; paired with it for that reason.
    InterfaceSpec { name: "zxdg_output_manager_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `zxdg_output_manager_v1.get_xdg_output`, so S must be able to name it too.
    InterfaceSpec { name: "zxdg_output_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run the consistency tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL — `` `wl_output` is in rayland_relay::interfaces::SUPPORTED but S has no linked descriptor `` and the matching C failure. **The table drove the failure, which is the invariant working.**

- [ ] **Step 3: Add the descriptors on both sides**

In `crates/rayland-s/src/wayland_client.rs`, add to the `use` block:

```rust
use wayland_client::protocol::wl_output::WlOutput;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1, zxdg_output_v1::ZxdgOutputV1,
};
```

Add to `interface_by_name`'s `match`:

```rust
        "wl_output" => WlOutput::interface(),
        "zxdg_output_manager_v1" => ZxdgOutputManagerV1::interface(),
        "zxdg_output_v1" => ZxdgOutputV1::interface(),
```

Add the same three names to `KNOWN_INTERFACE_NAMES`.

In `crates/rayland-c/src/wayland_proxy.rs`, add to the `use` block:

```rust
use wayland_protocols::xdg::xdg_output::zv1::server::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_server::protocol::wl_output::WlOutput;
```

Add to `server_interface_by_name`'s `match`:

```rust
        "wl_output" => WlOutput::interface(),
        "zxdg_output_manager_v1" => ZxdgOutputManagerV1::interface(),
```

Add `"wl_output"` and `"zxdg_output_manager_v1"` to `GLOBAL_INTERFACE_NAMES`. **Do not add `zxdg_output_v1`** — it is created by a request, not bound.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify against a real application**

Run: `cargo run -q -p rayland-c --bin rayland-c -- --print-globals > /tmp/wp0-bind-gap/wp0-offers.txt`
Run: `APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh`
Expected: gap drops from 14 to **12**, and neither `wl_output` nor `zxdg_output_manager_v1` appears.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "wp0: serve wl_output and zxdg_output_manager_v1

The highest-impact pair in the gap: without them a toolkit assumes scale 1 and
renders the wrong size on a HiDPI display. They describe S's monitor, which is
correct -- that is where the application is shown. Bind gap 14 -> 12."
```

---

### Task 6: `zxdg_decoration_manager_v1`

**Files:** the same three.

- [ ] **Step 1: Add to the table**

```rust
    // Negotiates server- versus client-side decorations. Without it winit falls back to drawing
    // its own, or to none, and the window looks unlike every other window on S's desktop.
    InterfaceSpec { name: "zxdg_decoration_manager_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `zxdg_decoration_manager_v1.get_toplevel_decoration`.
    InterfaceSpec { name: "zxdg_toplevel_decoration_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL, naming both interfaces.

- [ ] **Step 3: Add the descriptors**

S — `use`:

```rust
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
};
```

S — `match` and `KNOWN_INTERFACE_NAMES`:

```rust
        "zxdg_decoration_manager_v1" => ZxdgDecorationManagerV1::interface(),
        "zxdg_toplevel_decoration_v1" => ZxdgToplevelDecorationV1::interface(),
```

C — `use`:

```rust
use wayland_protocols::xdg::decoration::zv1::server::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1;
```

C — `match`, plus `"zxdg_decoration_manager_v1"` in `GLOBAL_INTERFACE_NAMES`:

```rust
        "zxdg_decoration_manager_v1" => ZxdgDecorationManagerV1::interface(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Run: `cargo run -q -p rayland-c --bin rayland-c -- --print-globals > /tmp/wp0-bind-gap/wp0-offers.txt && APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh`
Expected: gap 12 → **11**.

```bash
git add -A
git commit -m "wp0: serve zxdg_decoration_manager_v1 (bind gap 12 -> 11)"
```

---

### Task 7: `wp_cursor_shape_manager_v1`

This is the interface behind the cursor defect observed in the `solarsim` acceptance run: it lets a client *name* a cursor rather than supply pixels.

**Files:** the same three.

- [ ] **Step 1: Add to the table**

```rust
    // Lets a client name a cursor ("default", "text", …) instead of supplying pixels. This is the
    // interface behind the cursor that never appeared in the 2026-09-01 solarsim acceptance run.
    InterfaceSpec { name: "wp_cursor_shape_manager_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `wp_cursor_shape_manager_v1.get_pointer`.
    InterfaceSpec { name: "wp_cursor_shape_device_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL, naming both.

- [ ] **Step 3: Add the descriptors**

S — `use`:

```rust
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
```

S — `match` and `KNOWN_INTERFACE_NAMES`:

```rust
        "wp_cursor_shape_manager_v1" => WpCursorShapeManagerV1::interface(),
        "wp_cursor_shape_device_v1" => WpCursorShapeDeviceV1::interface(),
```

C — `use`, `match`, and `GLOBAL_INTERFACE_NAMES`:

```rust
use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
```
```rust
        "wp_cursor_shape_manager_v1" => WpCursorShapeManagerV1::interface(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Expected gap: 11 → **10**.

```bash
git add -A
git commit -m "wp0: serve wp_cursor_shape_manager_v1 (bind gap 11 -> 10)

The interface behind the cursor that never appeared in the 2026-09-01
solarsim acceptance run."
```

---

### Task 8: `wp_viewporter` and `wp_fractional_scale_manager_v1`

A pair for the same reason as Task 5: fractional scaling needs both the scale factor and the ability to say what source rectangle maps to what destination size.

**Files:** the same three.

- [ ] **Step 1: Add to the table**

```rust
    // Source-crop and destination-size for a surface; half of fractional scaling.
    InterfaceSpec { name: "wp_viewporter", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `wp_viewporter.get_viewport`.
    InterfaceSpec { name: "wp_viewport", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // The other half: tells the client the fractional scale the compositor wants.
    InterfaceSpec { name: "wp_fractional_scale_manager_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `wp_fractional_scale_manager_v1.get_fractional_scale`.
    InterfaceSpec { name: "wp_fractional_scale_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL, naming all four.

- [ ] **Step 3: Add the descriptors**

S — `use`:

```rust
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter};
```

S — `match` and `KNOWN_INTERFACE_NAMES`:

```rust
        "wp_viewporter" => WpViewporter::interface(),
        "wp_viewport" => WpViewport::interface(),
        "wp_fractional_scale_manager_v1" => WpFractionalScaleManagerV1::interface(),
        "wp_fractional_scale_v1" => WpFractionalScaleV1::interface(),
```

C — `use`, `match`, and both manager names in `GLOBAL_INTERFACE_NAMES`:

```rust
use wayland_protocols::wp::fractional_scale::v1::server::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::viewporter::server::wp_viewporter::WpViewporter;
```
```rust
        "wp_viewporter" => WpViewporter::interface(),
        "wp_fractional_scale_manager_v1" => WpFractionalScaleManagerV1::interface(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Expected gap: 10 → **8**.

```bash
git add -A
git commit -m "wp0: serve wp_viewporter and wp_fractional_scale_manager_v1 (bind gap 10 -> 8)"
```

---

### Task 9: `wp_presentation`

Of independent interest beyond the gap: it gives a **compositor-side** measurement of when a frame was actually shown, against which this project's own frame-time figures can be checked.

**Files:** the same three.

- [ ] **Step 1: Add to the table**

```rust
    // Presentation timestamps from the compositor. Beyond closing the gap, this is the only
    // compositor-side measurement of when a frame was actually shown — an independent check on
    // this project's own frame-time numbers.
    InterfaceSpec { name: "wp_presentation", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Created from `wp_presentation.feedback`, one per presented frame.
    InterfaceSpec { name: "wp_presentation_feedback", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL, naming both.

- [ ] **Step 3: Add the descriptors**

S — `use`:

```rust
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation, wp_presentation_feedback::WpPresentationFeedback,
};
```

S — `match` and `KNOWN_INTERFACE_NAMES`:

```rust
        "wp_presentation" => WpPresentation::interface(),
        "wp_presentation_feedback" => WpPresentationFeedback::interface(),
```

C — `use`, `match`, and `"wp_presentation"` in `GLOBAL_INTERFACE_NAMES`:

```rust
use wayland_protocols::wp::presentation_time::server::wp_presentation::WpPresentation;
```
```rust
        "wp_presentation" => WpPresentation::interface(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Expected gap: 8 → **7**.

```bash
git add -A
git commit -m "wp0: serve wp_presentation (bind gap 8 -> 7)"
```

---

### Task 10: The tail — subcompositor, activation, pointer, text input, fixes

Six interfaces of identical shape, grouped because each is a table entry plus one line per side and none needs its own review gate. `wp_linux_drm_syncobj_manager_v1` is **not** here — it is `Refused` in Task 11.

**Files:** the same three.

- [ ] **Step 1: Add to the table**

```rust
    // Subsurfaces: a video pane or GL canvas inside application chrome.
    InterfaceSpec { name: "wl_subcompositor", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "wl_subsurface", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Request or transfer focus ("open this window and raise it").
    InterfaceSpec { name: "xdg_activation_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "xdg_activation_token_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Pointer lock and confinement — needed by anything with a 3D camera.
    InterfaceSpec { name: "zwp_pointer_constraints_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "zwp_locked_pointer_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "zwp_confined_pointer_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Unaccelerated pointer deltas, the companion to a locked pointer.
    InterfaceSpec { name: "zwp_relative_pointer_manager_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "zwp_relative_pointer_v1", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Input methods — without it there is no CJK or emoji entry.
    InterfaceSpec { name: "zwp_text_input_manager_v3", max_version: u32::MAX, fds: FdPolicy::Transparent },
    InterfaceSpec { name: "zwp_text_input_v3", max_version: u32::MAX, fds: FdPolicy::Transparent },
    // Lets a client destroy a `wl_registry`, which libwayland's own globals helper uses.
    InterfaceSpec { name: "wl_fixes", max_version: u32::MAX, fds: FdPolicy::Transparent },
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --workspace`
Expected: FAIL, naming all twelve.

- [ ] **Step 3: Add the descriptors**

S — `use`:

```rust
use wayland_client::protocol::{wl_fixes::WlFixes, wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::{
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    zwp_relative_pointer_v1::ZwpRelativePointerV1,
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3, zwp_text_input_v3::ZwpTextInputV3,
};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::XdgActivationTokenV1, xdg_activation_v1::XdgActivationV1,
};
```

S — `match` and `KNOWN_INTERFACE_NAMES`:

```rust
        "wl_subcompositor" => WlSubcompositor::interface(),
        "wl_subsurface" => WlSubsurface::interface(),
        "xdg_activation_v1" => XdgActivationV1::interface(),
        "xdg_activation_token_v1" => XdgActivationTokenV1::interface(),
        "zwp_pointer_constraints_v1" => ZwpPointerConstraintsV1::interface(),
        "zwp_locked_pointer_v1" => ZwpLockedPointerV1::interface(),
        "zwp_confined_pointer_v1" => ZwpConfinedPointerV1::interface(),
        "zwp_relative_pointer_manager_v1" => ZwpRelativePointerManagerV1::interface(),
        "zwp_relative_pointer_v1" => ZwpRelativePointerV1::interface(),
        "zwp_text_input_manager_v3" => ZwpTextInputManagerV3::interface(),
        "zwp_text_input_v3" => ZwpTextInputV3::interface(),
        "wl_fixes" => WlFixes::interface(),
```

C — `use`:

```rust
use wayland_protocols::wp::pointer_constraints::zv1::server::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;
use wayland_protocols::wp::relative_pointer::zv1::server::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1;
use wayland_protocols::wp::text_input::zv3::server::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use wayland_protocols::xdg::activation::v1::server::xdg_activation_v1::XdgActivationV1;
use wayland_server::protocol::{wl_fixes::WlFixes, wl_subcompositor::WlSubcompositor};
```

C — `match`, and these six names added to `GLOBAL_INTERFACE_NAMES`:

```rust
        "wl_subcompositor" => WlSubcompositor::interface(),
        "xdg_activation_v1" => XdgActivationV1::interface(),
        "zwp_pointer_constraints_v1" => ZwpPointerConstraintsV1::interface(),
        "zwp_relative_pointer_manager_v1" => ZwpRelativePointerManagerV1::interface(),
        "zwp_text_input_manager_v3" => ZwpTextInputManagerV3::interface(),
        "wl_fixes" => WlFixes::interface(),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Verify and commit**

Expected gap: 7 → **1** (only `wp_linux_drm_syncobj_manager_v1` remains).

```bash
git add -A
git commit -m "wp0: serve subcompositor, activation, pointer constraints, relative pointer, text input, fixes

Bind gap 7 -> 1. The remaining one is wp_linux_drm_syncobj_manager_v1, which
carries a DRM syncobj fd and is Refused by design (Task 11)."
```

---

### Task 11: Record the two refusals

**Files:**
- Modify: `crates/rayland-relay/src/interfaces.rs`

- [ ] **Step 1: Add both entries as `Refused`**

```rust
    // --- Known, and deliberately NOT advertised -----------------------------------------------
    // Carries a DRM syncobj file descriptor: explicit GPU synchronisation between the application
    // and the compositor. That is a fourth member of the substitution family in its own right
    // (BufferToken sends a name, KeymapContent contents, ShmPool a size) and needs its own design,
    // not a table entry. Withholding it is safe today because clients fall back to implicit sync.
    InterfaceSpec {
        name: "wp_linux_drm_syncobj_manager_v1",
        max_version: u32::MAX,
        fds: FdPolicy::Refused("carries a DRM syncobj fd; cross-machine explicit sync is undesigned"),
    },
    // Clipboard and drag-and-drop transfer data over descriptors the APPLICATION creates, in both
    // directions, with a negotiated MIME type. solarsim does not bind it; rt does. Its own phase.
    InterfaceSpec {
        name: "wl_data_device_manager",
        max_version: u32::MAX,
        fds: FdPolicy::Refused("clipboard/DnD transfer over app-created fds; own phase"),
    },
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS. The consistency tests exempt `Refused` entries from needing a C descriptor, and S's `every_supported_interface_resolves` walks all of `SUPPORTED` — so **S must still name them**. Add to S's `match` and `KNOWN_INTERFACE_NAMES`:

```rust
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_protocols::wp::linux_drm_syncobj::v1::client::wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1;
```
```rust
        "wl_data_device_manager" => WlDataDeviceManager::interface(),
        "wp_linux_drm_syncobj_manager_v1" => WpLinuxDrmSyncobjManagerV1::interface(),
```

Re-run: `cargo test --workspace` → PASS.

- [ ] **Step 3: Verify C reports the refusals**

Run: `PROFILE=release TRIES=1 scripts/wp0-soak.sh`
Expected: the C log contains two lines:
```
registry: NOT advertising wp_linux_drm_syncobj_manager_v1 — carries a DRM syncobj fd; cross-machine explicit sync is undesigned
registry: NOT advertising wl_data_device_manager — clipboard/DnD transfer over app-created fds; own phase
```

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "wp0: record the two refusals, with reasons

A withheld optional global is correct Wayland behaviour; an UNRECORDED
absence is what let fourteen interfaces go missing unnoticed. Both are now
named at startup with why."
```

---

### Task 12: Acceptance

Acceptance is **not** a list of supported interfaces (spec §8).

**Prerequisite:** C and S must be mutually routable. As of 2026-09-02 they are not — apollo is on `172.16.20.10/24`, dop561 on `192.168.1.0/24`, with no working return path. Verify before starting:

```bash
ssh apollo 'timeout 5 bash -c "</dev/tcp/<S_IP>/22" && echo REACH_OK || echo REACH_FAIL'
```

- [ ] **Step 1: The bind gap contains only refusals**

Run: `cargo run -q -p rayland-c --bin rayland-c -- --print-globals > /tmp/wp0-bind-gap/wp0-offers.txt`
Run: `APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh`
Expected: exactly one MISSING line, `wp_linux_drm_syncobj_manager_v1`. Any `Transparent` interface in the gap is a failure.

- [ ] **Step 2: Named user-visible properties on the real two-machine run**

Run `solarsim` on milkv, displayed on dop561, via `scripts/milkv-demo.sh`. Check by looking, and **write down what was seen** — a screenshot into `docs/data/<date>-c4a-acceptance/`:

- the window is at the correct scale for dop561's display (not half-size or double-size);
- decorations are present;
- the cursor is visible over the window.

- [ ] **Step 3: No regression**

Run: `PROFILE=release TRIES=25 scripts/wp0-soak.sh`
Expected: clean-run rate within the existing bound (59/60 historically; 25/25 or 24/25 is consistent).

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Record the result**

Add a diary entry to `docs/DIARY.md` and update `docs/OVERVIEW.md` §6.1 and `project-map.js` (bumping `project.updated`), per `CLAUDE.md`'s binding rules. State the bind gap before and after, what was seen on screen, and anything that did **not** work.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "(c)4a acceptance: solarsim's bind gap is 14 -> 1, and the 1 is a refusal"
```

---

## Self-Review

**Spec coverage.** §3's two failure modes → Tasks 2 (mode 2 impossible) and 3 (mode 1 legible). §4's shared table → Task 1; two-sided tests → Tasks 2 and 3. §5's `FdPolicy` → Task 1, refusals in Task 11. §6.1 startup report → Task 3; §6.2 bind-gap report → Task 4. §7's ordering → Tasks 5–10, in the spec's order. §8's acceptance → Task 12. §9's mutation checks → Task 2 Step 2 and Task 4 Step 3. §10's "resolves is not works" → Task 12 Step 2, which is the only step that certifies user-visible behaviour. §11's network prerequisite → Task 12's prerequisite block.

**Known gap, stated rather than hidden.** §10's multi-output risk ("`wl_output` may be more than one object") has **no task**. It is out of scope by the spec's own wording — in scope only so far as the application sees the output it is displayed on — and a multi-monitor S is follow-up work. Task 12 Step 2 checks scale on the single display in use; if S ever has two monitors, this plan does not cover it.

**Type consistency.** `FdPolicy`, `InterfaceSpec`, `SUPPORTED`, `advertised()`, `spec_for()` are defined in Task 1 and used with those exact names in Tasks 2, 3, 4, 5–11. `interface_by_name` (S, pre-existing) and `server_interface_by_name` (C, Task 3) are deliberately different names for the two sides' maps, and `KNOWN_INTERFACE_NAMES` (S) and `GLOBAL_INTERFACE_NAMES` (C) are likewise distinct — C's holds only globals, S's holds every name its `match` answers to. Every `use` path was resolved against `wayland-protocols` 0.32.12 as vendored in this checkout, not from memory.
