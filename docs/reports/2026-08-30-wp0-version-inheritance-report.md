# Report to planning — version inheritance closed as a class, and a wrapper that was lying

**Session:** 2026-08-30, dop561 (see §6). **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-30-wp0-version-inheritance/`.

> **`vkgears` does not merely survive — it runs.** 345 attaches, 345 frame callbacks delivered,
> 10–13 fps, zero panics, `rayland-s` alive. **A second independent application now works end to end
> through WP0.** The guarded soak was 25/25 clean.
>
> **Your correction was right and my report was wrong**: the poisoned mutex is wayland-backend's own,
> not rayland's. Verified in the dependency's source before touching anything, then watched happen in
> a test.

---

## 1. Decision 3, verified rather than assumed

`ConnectionState { protocol: Mutex<ProtocolState>, … }`; `lock_protocol()` is
`self.protocol.lock().unwrap()` (`rs/client_impl/mod.rs:115`); `send_request` takes that guard at its
top and **every panic it raises fires with the guard held** — unknown opcode, interface mismatch, and
the version panic at `:368`. So a protocol violation poisons the dependency's own connection state and
the next backend call of any kind aborts. **No lock discipline of ours could ever have helped.**

## 2. What was fixed

**Version inheritance, as a class.** `IdMaps` records every object's version — seeded at bind with the
**capped** value (`version.min(g.version)`), inherited by every child — and `child_spec` is built from
**the sender**. The wire's version is logged (the gap *is* how much S capped) but decides nothing.
`synthesize_buffer` is unified onto the same rule rather than keeping its special case.

**Invariant, now in a comment:** *every object's version equals the capped version of the global it
descends from* — which is what makes one lookup sufficient.

**Prevention over catching.** A `precheck_request` refuses the two structurally predictable panics
(opcode out of range; child interface disagreeing with the descriptor) *before* calling
`send_request`, since catching them is not a recovery. It deliberately does **not** re-implement the
backend's argument-signature validation — duplicated validation that can drift is a worse failure than
the one it prevents.

**Honest failure.** On a panic that still gets through, the replay is declared **dead**: it says so,
sets a flag, and issues no further backend call. The vtest/ring relay continues — the application
loses its window, not its compute.

## 3. Verified, and by what evidence

| Claim | Evidence |
|---|---|
| A child of a capped bind is created at the capped version | `wayland_replay.rs::a_child_of_a_capped_bind_is_created_at_the_capped_version`, driving `handle_bind` + `handle_request` against a real compositor. **Skips** where nothing is capped, rather than reporting a green that means nothing |
| That test bites | **Mutation shown:** with `child_spec` built from the wire version it fails **on its own assertion** — *"the replay died … S capped xdg_wm_base to v5, the wire said v6"* |
| `vkgears` no longer crashes | `rayland-s` alive, **0** FATAL/panics, 2 cappings applied, chaining `xdg_wm_base` v5 → `xdg_surface` v5 → `xdg_toplevel` v5 |
| `vkgears` gets further than "not crashing" | **345 attaches, 347 commits, 345 `wl_callback.done` delivered, 10–13 fps** by its own FPS counter |
| No regression | `cargo test -p rayland-c -p rayland-s` green including the 71 s GPU loopback e2e; pure set **83** |
| `rayland-icosa-window` still refuses cleanly | `wl_shm unavailable…`, exit 0, `rayland-s` untouched, 0 panics |
| Guarded soak | **25 runs, 25 pass, 0 fail.** 236–393 attaches per 20 s run. Rate **< 12%** at 95% (rule of three; all n=25 can bound) |

## 4. The two bugs the mutation test found **in my own fix**

Worth reporting because neither would have been caught by review:

1. **I was still calling `flush()` after declaring the replay dead.** `flush` is a backend call, so it
   unwrapped the freshly poisoned mutex and aborted — the honest "the replay is dead" followed
   immediately by the crash it existed to avoid.
2. **The compositor-reader thread touches the same backend independently** and panicked alongside the
   main thread. A flag on the message thread stops only the message thread; the reader now shares an
   `AtomicBool` and stops.

Also worth stating: my **first** version test was a unit test that computed `child = sender_version` in
the test body and asserted it equalled the capped value. Green, and worthless — §6.4's hazard, hit by
me two days after writing it down. It is replaced, not patched.

## 5. The teardown guard — and my first attempt was the failure the brief warned about

I keyed it on "the first object destruction C observed", then ran it against the archived run 13
rather than trusting it. **The first destruction there is at line 62 of 4086**, because a
`wl_callback` is destroyed after every frame — the guard would have excused essentially the whole run.
That is exactly *"the exclusions are where a soak quietly stops measuring anything"*, built by me, one
session after being warned.

The interface census settles the right key: in that log `wl_callback` is destroyed **471 times**, while
`xdg_surface`, `xdg_toplevel` and `wl_surface` are destroyed **once each**, in a burst at the end.
Keying on those excuses a **15-line window instead of 4024**, and still turns run 13 from FAIL to PASS.

**What it excludes, stated exactly:** `wp-event[C] drop:` lines occurring at or after the first
destruction of an `xdg_toplevel`, `xdg_surface` or `wl_surface`. S-side drops are counted in full
regardless (S has no teardown marker of its own) — the conservative choice.

**And an honesty note:** **zero post-teardown drops occurred in the 25 runs**, so the soak provides
*no* evidence the guard works. The evidence is the archived-log validation above, and nothing else.

## 6. Deviations

1. **apollo is still down.** The soak and all runs are **loopback on dop561**. Valid for a failure rate
   of the relay and replay; it says nothing about latency, bandwidth, or a weak C machine, and the
   harness now records the topology so a figure cannot later be mistaken for the other case.
2. **`scripts/wp0-soak.sh` gained a loopback mode** to make that possible — the alternative was
   measuring nothing.
3. **Throughput is not comparable to the previous soak**: 236–393 attaches here (loopback) against
   261–489 there (two-machine). Different topologies; I am not claiming a regression or an improvement.
4. `RAYLAND_S_SHIP_PRESENTED` remains as declared debt, as instructed.

## 7. What remains unverified

| Open | What would settle it |
|---|---|
| The teardown guard actually excusing something in a live run | A run that reproduces the teardown race; none did in 25 |
| Any rate better than <12% | More runs; n=25 is what the session had time for |
| `vkgears` **rendering correctly** — I verified frames flow, not pixels | A human at a screen, or a capture |
| Whether the version fix holds against a compositor that caps *differently* | Only weston's v5 cap was exercised |
| The ~1 MB first-frame outlier, `wl_shm`, the keyboard, the commit gate | All still out of scope |
