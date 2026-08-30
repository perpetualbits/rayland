# The dropped `wl_keyboard.keymap` crashes applications — 2026-08-30

**This is a Rayland defect, and it is more serious than the record said.** The `wl_keyboard.keymap`
drop has been noted since the event-witness session as *"no relayed application will have a keyboard
until this gets a token-style substitution"* — a capability gap. It is worse than that: **it crashes
applications that use a seat.**

## The chain, from our own witness plus a backtrace

```
rayland-s: WP0 replay bound `wl_seat` v4 (app obj 5)
[wp-event][S] emit app_obj=5 wl_seat.capabilities        <- app learns a keyboard exists
[wp-proxy]    objects+ app_obj=3 wl_keyboard             <- app calls wl_seat.get_keyboard
[wp-event][S] from-compositor wl_keyboard.keymap
[wp-event][S] drop:carries-fd wl_keyboard.keymap         <- WE DROP THE KEYMAP
[wp-event][C] delivered app_obj=3 wl_keyboard.repeat_info <- and keep delivering keyboard events
                                                            SIGSEGV
```

Backtrace at the crash (`vkgears` through WP0 against COSMIC):

```
#0  xkb_state_update_mask () at libxkbcommon.so.0
#3  ffi_call () at libffi.so.8
#6  wl_display_dispatch_queue_pending () at libwayland-client.so.0
```

`xkb_state_update_mask` on an xkb state that was never created, because the keymap that would have
created it was dropped.

## Why it looked like "vkgears is fragile"

| S's compositor | seat? | keymap sent? | result |
|---|---|---|---|
| headless weston (as launched here) | **no `wl_seat`** | never | **vkgears runs**, ~15 fps |
| COSMIC | yes | sent, **dropped by us** | **SIGSEGV** |

Against a seatless compositor no seat events flow at all, so nothing depends on the missing keymap.
That is the entire reason the "works against headless weston, dies against COSMIC" table looked like a
property of the compositor. It is a property of **whether a seat exists to expose our own gap**.

## The shape of the defect

We do three things that are individually defensible and jointly fatal:

1. The C proxy **advertises `wl_seat`** unconditionally (`wayland_proxy.rs:998`).
2. S **relays `wl_seat.capabilities`**, so the application creates a `wl_keyboard`.
3. S **drops `wl_keyboard.keymap`** because it carries a file descriptor — correct in isolation, since
   an fd cannot cross a network — and then **keeps relaying the keyboard's other events**.

The application is handed a half-initialised object and then fed events that assume it is complete.
**Dropping an event whose dependants are still delivered is the bug**, not the drop itself.

## Options, none taken here (no prompt, and this is a design decision)

- **Substitute the keymap**, as the buffer path substitutes a token: ship the keymap *content* rather
  than the fd and have C synthesise a local memfd. It is a bounded string, unlike a swapchain.
- **Suppress the whole keyboard**: if the keymap cannot cross, do not relay `capabilities` with the
  keyboard bit set, so no application ever creates a keyboard it cannot use.
- **Do not advertise `wl_seat`** at all until one of the above lands. Note this makes applications like
  `vkgears` work by removing the trigger rather than the cause.

The second and third are cheap and stop the crash; the first is what the capability actually needs.
