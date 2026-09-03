# (c)4a: Wayland protocol breadth — what real applications actually ask for

**Status:** design, 2026-09-02. **IMPLEMENTED 2026-09-03 — Tasks 1–11 of 12 built and committed;
Task 12 (acceptance) is owed and needs the two machines.** The problem statement below describes the
state at design time (a five-global registry); WP0 now advertises **eighteen** globals and states two
refusals with their reasons. Left as written rather than rewritten, per the house pattern for a
document whose moment has passed. Plan and outcome:
[`../plans/2026-09-02-c4a-protocol-breadth.md`](../plans/2026-09-02-c4a-protocol-breadth.md).
Supersedes nothing; this is the first spec for arc (c) phase 4.
**Acceptance application:** `solarsim` (unmodified wgpu/winit, Vulkan).
**Explicitly deferred to a later phase:** OpenGL via Zink, and with it `rt`.

---

## 1. The problem, in one paragraph

WP0 works: an unmodified Vulkan application runs on C, is rendered by S's GPU, and appears in its own
window on S's screen. But it works on a **five-global** Wayland registry, and a real toolkit
application asks for nineteen. The missing fourteen do not crash anything — Wayland globals are
optional, so the application quietly does without them. The result is a program that runs while
lacking its display's scale factor, its window decorations, its cursor shape, fractional scaling and
presentation timing, **and nothing anywhere reports that it is degraded**. This phase closes that gap
and, more importantly, ends the mechanism that let it go unnoticed.

---

## 2. What is actually missing, measured rather than guessed

Captured 2026-09-02 by running each application against the live COSMIC session with `WAYLAND_DEBUG=1`
and extracting every `wl_registry.bind`. This is evidence, not a reading of the protocol catalogue.

| | renderer | globals bound | offered by WP0 today |
|---|---|---|---|
| `solarsim` | **Vulkan** (`mesa vk display queue`) | **19** | **5** |
| `rt` | GL/EGL (`mesa egl surface queue`) | 25 | 5 |

WP0 advertises exactly five globals, in `crates/rayland-c/src/wayland_proxy.rs`: `wl_compositor`,
`xdg_wm_base`, `zwp_linux_dmabuf_v1`, `wl_seat`, `wl_shm`.

`solarsim` asks for fourteen more:

`wl_output` · `zxdg_output_manager_v1` · `zxdg_decoration_manager_v1` · `wp_cursor_shape_manager_v1` ·
`wp_viewporter` · `wp_fractional_scale_manager_v1` · `wp_presentation` · `wl_subcompositor` ·
`xdg_activation_v1` · `zwp_pointer_constraints_v1` · `zwp_relative_pointer_manager_v1` ·
`zwp_text_input_manager_v3` · `wp_linux_drm_syncobj_manager_v1` · `wl_fixes`

**Both applications are `winit` applications and their lists overlap heavily.** That is what makes
this a specification rather than one program's quirk: the demand comes from the toolkit, so serving
it serves the class.

### 2.1 A finding that reorders the roadmap, and a correction

`rt` — the repository owner's own terminal — **cannot run over Rayland at all today, and not for want
of protocol coverage.** `crates/rt/src/backend.rs::choose_backend` returns `BackendKind::Gl` for any
non-X11 display, and `rt`'s protocol trace contains **zero** Vulkan lines against 2,143 on Mesa's EGL
queues. Rayland relays *Vulkan* (via Venus); an OpenGL application emits no Venus stream to capture.

This was found while scoping, after `rt` had been proposed as the acceptance application on the
strength of it using `winit` — an inference, not a fact, and it was wrong. It is recorded here because
the conclusion travels: **choosing `rt` as an acceptance application makes GL-via-Zink a
prerequisite**, inverting the phase order. `rt` remains the north star and becomes the acceptance
application for the Zink phase; `solarsim` is the acceptance application for this one.

---

## 3. The real defect is that the supported set is written down twice

The interfaces WP0 supports are declared in **two places, in two crates, in two type systems**, with
nothing tying them together:

- **C** decides what the application may see: `create_global::<WlCompositor>(&handle, …)` and four
  siblings, using `wayland-server`'s generated descriptors.
- **S** decides what it can replay: `interface_by_name` in `crates/rayland-s/src/wayland_client.rs`,
  a hand-written `match` over `wayland-client`'s generated descriptors.

They drifted, and the project has already paid for it. `wl_shm` was added to C on 2026-09-01 and
**forgotten in S**. C forwarded `create_pool` perfectly; S logged `no linked descriptor for wl_shm;
bind skipped`; the application carried on because its GPU frames go through dma-buf. The only symptom
was a cursor that never appeared, and the only detector was a human looking at a screen.

Note the two distinct failure modes, because the fix differs:

1. **Not advertised by C** — the application never sees the global and adapts. Correct Wayland
   behaviour, and the state of all fourteen today. The defect is that it is *invisible*.
2. **Advertised by C, unreplayable by S** — the application binds and the bind is dropped mid-session.
   This is the `wl_shm` bug, and it is a genuine inconsistency rather than a degradation.

Mode 2 must be made impossible. Mode 1 must be made *legible*.

---

## 4. Approach: one shared declaration, two derived tables

A single const table in `rayland-relay` — pure data, no GPU, no sockets, already a dependency of both
`rayland-c` and `rayland-s`, and therefore the only place both sides can honestly agree:

```rust
pub struct InterfaceSpec {
    /// The wire name, e.g. "wl_output".
    pub name: &'static str,
    /// The highest version WP0 will advertise, independent of what the descriptor supports.
    pub max_version: u32,
    /// What this interface does about file descriptors. See §5.
    pub fds: FdPolicy,
}

pub const SUPPORTED: &[InterfaceSpec] = &[ /* … */ ];
```

- **C** iterates `SUPPORTED`, advertising every entry whose `fds` is not `Refused`.
- **S** does *not* iterate it to build its map — it cannot, because the descriptor is a different Rust
  type on each side — but it is **tested against it**: every name in `SUPPORTED` must resolve in
  `interface_by_name`, and `interface_by_name` must contain no name outside `SUPPORTED`.

That second test is the one that would have caught `wl_shm`, and it is worth being precise about why
it works where the existing test did not. The existing test enumerates the eleven names it expects and
asserts they resolve — so it can only find a name someone remembered to add to *both* the code and the
test. The new test compares S's map against a list maintained for a different purpose (C's
advertisement), so forgetting one side is a failure rather than a silence.

**It cannot be a single table of descriptors.** `WlCompositor` in `wayland-server` and `WlCompositor`
in `wayland-client` are different types with different `&'static Interface` values. Sharing the names,
versions and policy is the most that is true; each side keeps its own name→descriptor mapping, and the
tests keep them honest.

### Alternatives considered and rejected

**Hand-extend the existing tables (add fourteen entries and move on).** Smallest change, matches the
current design exactly, every step independently testable. Rejected as the *spine* — though its
ordering is adopted in §7 — because it fixes the fourteen instances today's evidence names and leaves
the mechanism that hid them. The fifteenth interface some future application binds would be skipped as
silently as `wl_shm` was, and the project would learn about it the same way: by a person noticing
something missing on a screen.

**Fully generic, protocol-XML-driven relay.** Parse the Wayland XML and marshal any interface with no
per-interface code at all. Rejected: it cannot be honest about file descriptors. An interface carrying
an fd needs a *designed* substitution (see §5), and a generic marshaller either drops fds silently —
the exact class of bug this phase exists to end — or guesses. It would also create false confidence:
"resolves" is not "works", and a table that answers every question makes an unsupported interface look
supported.

---

## 5. The file-descriptor policy, made explicit rather than implied

This project's founding constraint is that **a file descriptor cannot cross a network**. WP0 already
answers that three times, and the answers form a family worth naming:

| existing case | what crosses instead |
|---|---|
| swapchain `wl_buffer` | **`BufferToken`** — a *name* for a resource S already holds |
| `wl_keyboard.keymap` | **`KeymapContent`** — the *contents*, remitted into a fresh sealed memfd on C |
| `wl_shm.create_pool` | **`ShmPool`** — a *size*; contents follow separately as they change |

Every interface in `SUPPORTED` declares one of three dispositions:

- **`Transparent`** — carries no descriptors. Requests and events relay unchanged. **Thirteen of
  `solarsim`'s fourteen**, and every interface in §7's ordering.
- **`Substituted(kind)`** — carries a descriptor with a designed replacement, as above.
- **`Refused(reason)`** — carries a descriptor with **no** designed replacement. **C does not
  advertise it**, so the application falls back cleanly, and C logs the withholding *with its reason*
  at startup.

`Refused` is the important addition, and the reasoning behind it is worth stating plainly: not
advertising an optional global is *correct* Wayland behaviour, not a bug. Applications are built to
cope. The bug today is not that fourteen globals are absent — it is that their absence is unrecorded,
so nobody can tell a deliberate omission from an oversight. `Refused` converts a silence into a
statement.

Two interfaces are `Refused` in v1. Note they are not both drawn from `solarsim`'s fourteen — the
first is, the second is one `rt` binds and `solarsim` does not, listed here because the policy has to
be decided once rather than twice:

- **`wp_linux_drm_syncobj_manager_v1`** — carries a DRM syncobj file descriptor. This is explicit
  cross-machine GPU synchronisation and a fourth member of the substitution family in its own right,
  not a table entry. It deserves its own design.
- **`wl_data_device_manager`** — clipboard and drag-and-drop transfer data over descriptors the
  *application* creates, in both directions, with a negotiated MIME type. `solarsim` does not bind it;
  `rt` does. Its own phase.

---

## 6. Instrumentation — the first deliverable, shipped before any interface is added

Two pieces, and they are the part of this spec most likely to still be earning its keep in six months.

### 6.1 C states its registry decision at startup

One log block naming every global advertised (with version), and every `Refused` entry with its
reason. Today the registry is five `create_global` calls in source and nothing at runtime; after this,
what the application was offered is in the session log next to what it did.

### 6.2 The bind-gap report

A script that runs an application twice — once against a real compositor, once against WP0 — captures
`wl_registry.bind` from each, and diffs them. Output: *what this application asked for that WP0 did
not offer.*

This generalises the manual capture that produced §2, and it answers the question for **any**
application, including ones nobody has yet tried. It is the acceptance instrument (§8), and it exists
because the alternative — a list of supported interfaces — provably cannot find the entry you forgot.

**Its own trap, stated so the next person does not fall into it:** the two runs must use the *same
application binary and the same compositor generation*, and the real-compositor run must be against a
compositor that advertises everything (the live session), not headless weston. Headless weston
advertises no `wl_seat`, and a sweep against it is structurally blind to an entire class of interface —
which this project has already been caught by once.

---

## 7. Which interfaces, and in what order

Ordered by user-visible effect, from `solarsim`'s measured list. Each lands with the §9 tests.

1. **`wl_output` + `zxdg_output_manager_v1`** — scale factor, geometry, mode, refresh. Without them a
   toolkit assumes scale 1; on a HiDPI display the window is visibly the wrong size. Highest impact,
   and the pair is natural: `zxdg_output_manager_v1` exists to add logical geometry to `wl_output`.
   **Semantic note:** these describe **S's** monitor, and relaying S's real values is correct — the
   application is being displayed there, so that is the truth it should see.
2. **`zxdg_decoration_manager_v1`** — negotiates server- versus client-side decorations.
3. **`wp_cursor_shape_manager_v1`** — lets the client name a cursor instead of supplying pixels. This
   is the interface behind the cursor defect already observed in the `solarsim` acceptance run.
4. **`wp_viewporter` + `wp_fractional_scale_manager_v1`** — fractional scaling; a pair for the same
   reason as (1).
5. **`wp_presentation`** — presentation timestamps. Of independent interest: it would give the project
   a *compositor-side* measurement of frame delivery, against which its own numbers can be checked.
6. **`wl_subcompositor`**, then `xdg_activation_v1`, `zwp_pointer_constraints_v1`,
   `zwp_relative_pointer_manager_v1`, `zwp_text_input_manager_v3`, and `wl_fixes`.

All are `Transparent`. None requires a new relay message; they exercise the existing request/event
path, which is why the ordering is by *value* rather than by difficulty.

---

## 8. Acceptance criteria

Acceptance is **not** a list of supported interfaces. The repository has already recorded why: *"a
test that lists the things it supports cannot find the one you forgot."*

1. **The bind-gap report for `solarsim` against WP0 contains only `Refused` entries**, each naming its
   reason. Any `Transparent` interface appearing in the gap is a failure.
2. **Named user-visible properties hold on the real two-machine run** — `solarsim` on milkv, rendered
   and displayed on dop561: the window is at the correct scale for dop561's display, decorations are
   present, and the cursor is visible. Each is checked by looking, and recorded with what was seen.
3. **The two-sided consistency tests pass** (§9), which is the `wl_shm` class closed.
4. **No regression**: `vkcube` and `vkgears` still run end to end, and the WP0 soak's failure rate is
   unchanged within its existing bound.

Explicitly **not** claimed by this phase: any frame-rate improvement, clipboard, drag-and-drop,
cross-machine explicit GPU sync, or that `rt` runs.

---

## 9. Testing

- **Two-sided consistency** — `SUPPORTED` versus S's `interface_by_name`, in both directions. This is
  the test that closes the `wl_shm` class, and it must be written so it fails if either side is edited
  alone. Verify that by mutation: delete an entry from S's map and confirm the test fails; add one not
  in `SUPPORTED` and confirm it fails too.
- **Policy coverage** — every `SUPPORTED` entry has an `FdPolicy`, and every `Refused` entry is absent
  from what C advertises. Mutation-check by flipping one entry to `Transparent` and asserting C would
  then advertise it.
- **Per-interface relay tests** — for each interface added in §7, a test that a bind and a
  representative request round-trip through the recorder. These guard regressions; they are not
  acceptance evidence, and the spec says so to stop them being mistaken for it.
- **The bind-gap report is itself tested** against a known-short registry, so a report that silently
  produces an empty diff cannot pass for success. *A number that is identically zero for a whole class
  of input is not a measurement.*

---

## 10. Risks

- **"Resolves" is not "works."** Adding an interface to the tables makes a bind succeed; it does not
  prove events flow back correctly or that the application does anything sensible with them. §8's
  user-visible properties exist because of this, and per-interface tests must not be quoted as if they
  discharged it.
- **Version negotiation.** Each interface is advertised at a version, and a child object inherits its
  parent's. This project has hit version-inheritance bugs **three times** already. `max_version` in
  `SUPPORTED` exists so a cap is a data change rather than a code change, and the existing inheritance
  machinery is reused rather than re-derived.
- **`wl_output` may be more than one object.** A compositor advertises one global per monitor.
  Multi-output handling is real work and is **in scope only to the extent that the application must
  see at least the output it is displayed on**; a multi-monitor S is a follow-up.
- **The gap report depends on `WAYLAND_DEBUG` output format**, which differs between libwayland builds
  (`obj@id` versus `obj#id` — both were seen on 2026-09-02, and the first attempt at this capture
  extracted nothing because of it). The parser must accept both and **fail loudly on zero matches**
  rather than reporting an empty gap.

---

## 11. Dependencies

- `wayland-protocols` 0.32.12 is already a dependency of both crates; the staging and unstable
  protocols needed here are in it. This phase needs **feature flags, not new dependencies**.
- Nothing in this phase touches the Venus command relay, the (c)2 return path, or the engine. It is
  confined to the WP0 Wayland proxy on both sides plus a data table in `rayland-relay`.
- The two-machine acceptance run needs C and S on a mutually routable network. As of 2026-09-02 they
  are not: apollo sits on `172.16.20.10/24` and dop561 on `192.168.1.0/24` with no working return
  path.
