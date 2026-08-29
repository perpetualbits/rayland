# WP0 event-return-path witness — captured run, 2026-08-29

The evidence behind [`docs/reports/2026-08-29-wp0-event-witness-report.md`](../../reports/2026-08-29-wp0-event-witness-report.md).

Produced by `scripts/wp0-vkcube-two-machine.sh` with `RAYLAND_S_EVENT_LOG=1` on S and
`RAYLAND_WP_LOG=1` on C: vkcube on apollo, its Wayland session replayed onto dop561's
COSMIC compositor. **Two runs were taken and both behaved identically; this is the second**,
which carries the fuller instrument (the object-map witness on both ends).

| File | What it is |
|---|---|
| `rayland-s.log` | S's daemon. `[wp-event][S]` lines are the witness: every event S's compositor emitted on a replayed object, and whether it was emitted toward C, suppressed, or dropped. |
| `rayland-c.log` | C's daemon and proxy. `[wp-event][C]` lines are the other half: what reached the application, what was dropped. `objects+`/`objects-` track the delivery map. |
| `vkcube.log` | The application's own output — two lines, no error. It does not fail; it waits. |
| `cube-on-dop561.png` | **The application's window on S's screen**, cropped from a full-screen capture. The cube is rendered by S's GPU from an app running on another machine. It is a *still* frame: the same 450×450 interior was pixel-identical (0 of 202 500 differing) across two captures 17 s apart. |

The full-screen captures are deliberately **not** committed — they show the machine owner's
other windows. Only the application's own window is kept.

## The answer, in one grep

```
$ grep 'wl_callback' rayland-s.log       # S's compositor emitted done TWICE
[wp-event][S] emit s_obj=13 app_obj=24 wl_callback.done args=1
[wp-event][S] emit s_obj=13 app_obj=24 wl_callback.done args=1

$ grep -E 'app_obj=24' rayland-c.log     # C delivered the first and lost the second
[wp-proxy]    objects+ app_obj=24 wl_callback          <- frame callback #1 created
[wp-event][C] delivered app_obj=24 wl_callback.done    <- #1 delivered; done DESTROYS it
[wp-proxy]    objects+ app_obj=24 wl_callback          <- #2 created, REUSING id 24
[wp-proxy]    objects- app_obj=24 wl_callback (was_known=true)   <- #1's destroyed() fires LATE
[wp-event][C] drop:unknown-object app_obj=24 opcode=0: no live proxy object
```

`destroyed()` for callback #1 runs *after* callback #2 was registered under the same
recycled id, and removes it. The application then waits forever for a frame callback that
was delivered to nothing.
