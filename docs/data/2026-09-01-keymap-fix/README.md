# The keymap defect, fixed — and what it uncovered behind it

## The defect

`wl_keyboard.keymap` hands a client a **file descriptor** to a read-only mapping holding the XKB
keymap as text. An fd cannot cross a network, so S dropped the whole event:

```
[wp-event][S] drop:carries-fd s_obj=5 wl_keyboard.keymap
```

The cost was not "no keyboard". An application that binds `wl_seat` and creates a `wl_keyboard`
**waits for its keymap**. `vkgears`, relayed to a compositor that advertises a seat, built its
swapchain and then never drew a frame. `vkcube` escaped only because it ignores the keymap — and
**headless weston, which every sweep in this project used, advertises no `wl_seat` at all**, so the
defect was invisible to the entire measurement harness for weeks. It appeared the moment the demo
pointed at the owner's live COSMIC session.

## The fix: ship the contents, mint a new fd

The mirror of buffer-by-token. There is nothing on S to *name* here — the keymap is **data**, a few
tens of KiB of text, immutable for the life of the keyboard. So:

- **S** reads the descriptor's contents and sends `WaylandArg::KeymapContent(bytes)` in place of the
  `Fd` argument.
- **C** creates a fresh sealed `memfd` holding those bytes and hands the application *that*
  descriptor. The application mmaps a read-only fd of `size` bytes holding the keymap — exactly what
  the protocol promises — and cannot tell the difference.

Scoped deliberately: this is **not** a general fd substitution. It carries bytes, so it is correct only
for a descriptor whose *contents are the whole payload*. Anything naming a GPU buffer or a sync file
has identity beyond its bytes and is still dropped.

## The bug inside the fix, and how it announced itself

The first version used `dup` + `read_to_end`, and `vkgears` died with **exit 135 — SIGBUS**.

`dup` does **not** give an independent file offset: the copy shares the *open file description*,
offset included. So `read_to_end` started wherever the compositor's descriptor happened to point,
could return fewer bytes than the file holds — and left the compositor's own offset at EOF. The
event's `size` argument travels unchanged, so a short read means the application maps `size` bytes
over a shorter file and **faults reading past the end**.

A comment in that first version asserted the opposite ("reads from the duplicate, whose file offset it
owns"). It was wrong, and the confident assertion is what made the bug hard to see.

The fix is `read_at` (`pread`): an explicit offset, no shared state touched, no dup needed. Verified:

```
rayland-s: WP0 keymap: relaying 35581 bytes of keymap content
[wp-event][C] delivered app_obj=3 wl_keyboard.keymap args=3 scalars=[1,35581]
```

**35,581 relayed against 35,581 advertised**, and the length is now logged unconditionally precisely
because a mismatch is a SIGBUS in the application rather than an error anyone would see.

Guarded by two tests, both mutation-checked: the synthesized fd reads back byte-identical (catches a
half-written keymap), and it is positioned at offset 0 (catches the forgotten rewind — a bug that
would reproduce only on toolkits that `read()` rather than `mmap()`).

## Status after the fix: better, and still not rendering

| | before | after |
|---|---|---|
| `wl_keyboard.keymap` | dropped | **delivered, 35,581 bytes** |
| event drops, either side | 1 | **0** |
| application | hung, then SIGBUS on the first fix | **alive, no crash** |
| frames drawn | 0 | **0** |

**`vkgears` still does not render, and the remaining blocker is a different one**, now localised: it
receives its configure, builds four `wl_buffer`s, attaches and commits **twice**, and then receives
**zero `wl_buffer.release` events** — so it blocks waiting for a buffer to come back and never draws
again. `vkcube` on the same path receives ~1,300 releases, so the return path works in general.

Note also that vkgears never requests a frame callback at all: the two `opcode 3` requests in its log
are `xdg_toplevel.set_title` and `xdg_surface.set_window_geometry`, not `wl_surface.frame`. An opcode
is an index into one interface's request list, and reading it without the interface is how that was
briefly misdiagnosed here.

**This is the next thing to chase, and it is not the keymap.**
