# Report to planning — where a WP0 frame's 66 ms go

**Session:** 2026-08-30, dop561, **loopback throughout**. **Branch:** `wp0-wayland-proxy`.
**Evidence:** `docs/data/2026-08-30-wp0-frame-time/`.

> **≈4.4 synchronous round trips per frame** — the number that transfers to a real network.
> **The native ceiling is 39.4 fps, not 60, and it is pure compositor pacing.**
> Budget: 0.49 ms GPU + ~24.9 ms pacing + **~40.4 ms Rayland** = 65.8 ms.
> **Nothing fixed.**

---

## 1. Round trips per frame (criterion 1)

362 presented frames, C's own per-channel counters:

| channel | per frame | blocking? |
|---|---:|---|
| **S→C replies** | **4.43** | **yes** — answers to blocking requests |
| S→C control | 2.00 | partly |
| C→S ring / inline | 3.24 / 3.24 | no |
| **C→S blob sync** | **72.50** | no — asynchronous push |
| S→C blob sync | 9.49 | no |

**≈4.4 round trips per frame**, ≈6.4 counting control. As an *n × RTT* floor:

| link | RTT | added/frame | effect on 65.8 ms |
|---|---:|---:|---|
| this LAN | 0.5 ms | 2.2 ms | invisible |
| same city | 5 ms | 22 ms | +33% |
| another country | 30 ms | 132 ms | 3× worse |
| transatlantic | 80 ms | 352 ms | fatal |

**This is a good number.** LAN and metro links are fine; intercontinental is not, and 4.4 is a far more
tractable target than the 72.5 forward messages, which cost CPU and bandwidth rather than latency.

## 2. Baselines (criterion 2) — and they changed the question

| configuration | ms/frame | fps |
|---|---:|---:|
| native, IMMEDIATE (no vsync) | **0.49** | 2037 |
| native, FIFO (default) | **25.37** | 39.4 |
| through WP0, FIFO | **65.8** | 15.2 |

Steady state by slope (300 vs 900 frames; startup 0.12 s). Three native FIFO runs within 0.1 fps.

**The ceiling is 39.4 fps and it is entirely pacing** — GPU work is 0.49 ms, so 24.9 ms of every
*native* frame is waiting for weston's callback. Comparing Rayland against 60 fps would have charged
it 4.6× when the honest figure is 2.6×.

**On `vkgears` — this section's original claim was WRONG and is corrected below** (the original
sentence is preserved in the diary; the corrected facts are what should be acted on):

> ~~It segfaults natively, so it is a fragile binary unusable for attribution.~~

The root cause is specific, not general. mesa-demos 9.0.0 dereferences the `wl_seat` global
unconditionally (`vulkan/wsi/wayland.c:236`), so it dies against any **seatless** compositor — and the
headless weston used here advertises no seat, which S's own log states. **Natively against COSMIC it
runs at 60.8–61.1 FPS**, so a baseline *is* obtainable.

And its failure *through Rayland* is **a Rayland defect, not vkgears' fragility** — see §7.

Every figure in this report is `vkcube`'s, which is unaffected.

## 3. The budget (criterion 3)

| component | ms/frame | share | established by |
|---|---:|---:|---|
| GPU render + present | 0.49 | 0.7% | native IMMEDIATE |
| Compositor pacing | ~24.9 | 38% | native FIFO − IMMEDIATE |
| **Rayland** | **~40.4** | **61%** | WP0 FIFO − native FIFO |
| observed | 65.8 | | |

Inside Rayland's 40 ms, existing `RLTRACE` stations on one clock (291 frames):

| interval | n | median | p10 | p90 | max |
|---|---:|---:|---:|---:|---:|
| S ships blob → C has it | 932 | 2.13 ms | 1.49 | 3.51 | 33.1 |
| S receives delta → engine consumes | 1004 | 3.96 ms | 2.32 | 13.98 | 141.2 |
| S consumed → C is told | 1319 | 7.90 ms | 5.09 | 14.51 | 149.8 |

They occur 3–5× per frame and overlap, so they **locate** the 40 ms rather than sum to it. **The shape
is the finding:** p90 is 2–4× the median, maxima are 30–150 ms. This is a mostly-fast pipeline with a
heavy tail, not a uniformly slow one — different system, different cause.

**Unaccounted for, explicitly:** the 40 ms is located, not itemised. The application's `vkQueueSubmit`
and the `wl_buffer` commit have **no trace stations**, so two segments are uninstrumented. And
`--present_mode 0` through WP0 yields no frames — *"Present mode specified is not supported"*, Venus
does not expose IMMEDIATE — so pacing cannot be subtracted on the Rayland side as it can natively.

## 4. Ruled OUT (criterion 4)

| Suspect | Ruled out by |
|---|---|
| GPU render time | 0.49 ms/frame — 0.7% of the frame |
| The network | loopback throughout; ship→receive is still 2.13 ms with no network |
| Bandwidth | ~3.6 KB/frame out, 219 B/frame back |
| Compositor pacing as the whole story | accounts for 25.4 of 65.8 ms |
| **Polling granularity alone** | 500 µs / 200 µs sleeps cannot make 4–8 ms intervals |

**Still live:** forward blob-sync volume (72.5 msgs/frame) — corroborated from an unexpected
direction, since milkv's ~3.7×-slower core produced almost exactly 3.7× fewer frames, the signature of
per-message cost. And whatever produces the heavy tail.

## 5. Corrections

**The dated qualifier the brief asked for is attached** in `OVERVIEW.md` wherever "works end to end"
appears: that run was **loopback**, and the two-machine `vkgears` confirmation is **owed**. It may be
unobtainable as stated, since vkgears does not run natively at all; `vkcube` *has* been run
apollo→dop561 and milkv→dop561 with a window on screen, and that is the claim that survives.

## 6. The smallest useful next step

**Add two trace stations** — the application's submit arriving at C, and the `wl_buffer` commit — and
re-run this. That closes the two uninstrumented segments and turns "~40 ms is Rayland's" into an
itemised list. It is additive, it needs only loopback, and it is the difference between knowing where
the time is and knowing what to change.

Then, and only then, the forward blob-sync coalescing — with **milkv as the machine that shows its
value**, since on apollo the equivalent readback change measured as nothing.

---

## 7. Addendum, same day — the dropped keymap **crashes applications**

Investigating a peer session's correction turned up a defect more serious than anything else in this
report. Full write-up and logs: `docs/data/2026-08-30-wp0-frame-time/keymap-drop-crashes-applications.md`.

**`wl_keyboard.keymap` has been recorded since the event-witness session as a capability gap** — *"no
relayed application will have a keyboard"*. It is not a gap. It is a crash:

```
emit  wl_seat.capabilities            -> the app learns a keyboard exists and creates one
drop:carries-fd wl_keyboard.keymap    -> WE DROP THE KEYMAP
delivered wl_keyboard.repeat_info     -> and keep delivering keyboard events
                                         SIGSEGV in xkb_state_update_mask (backtrace confirmed)
```

We advertise `wl_seat`, relay `capabilities` so the application creates a `wl_keyboard`, drop the one
event that initialises it, and then keep feeding it dependent events. **Dropping an event whose
dependants are still delivered is the bug**, not the drop.

It also explains the "vkgears works on headless weston, dies on COSMIC" table in the earlier report:
seatless weston never sends a keymap, so nothing depends on the missing one. That table was measuring
*whether a seat exists to expose our gap*, not a property of compositors.

**Cheap mitigations** (not applied — no prompt, and the choice is a design decision): suppress the
keyboard bit in relayed `capabilities`, or stop advertising `wl_seat`, either of which stops the crash.
**The real fix** is substituting the keymap's *content* the way the buffer path substitutes a token — it
is a bounded string, unlike a swapchain.