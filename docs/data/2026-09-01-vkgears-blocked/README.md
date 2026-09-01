# Why vkgears never gets `wl_buffer.release` — it never asks for one

## The short answer

**The question contained a wrong premise, and mine was the wrong premise.** `vkgears` is not waiting
for a buffer release. It **never attaches a buffer at all**. It blocks inside Venus, in the Vulkan
swapchain path, before it ever presents — so there is nothing for the compositor to release.

## How the wrong premise got there, twice

I counted `forward obj N opcode 1` across every object and called the result "attaches". **An opcode
is an index into one interface's request list**, so opcode 1 is `wl_surface.attach` *only on a
`wl_surface`*. The two hits were `wl_seat.get_keyboard` and `xdg_surface.get_toplevel`. I made the
identical mistake minutes earlier reading `opcode 3` as `wl_surface.frame` when it was
`xdg_toplevel.set_title` and `xdg_surface.set_window_geometry`.

This project wrote that lesson down months ago — *"a protocol id is a slot number, not an identity"* —
about **ids**. The same sentence is true of **opcodes**, and neither the tooling nor I applied it.
Object ids are resolved in the log (`objects+ app_obj=6 wl_surface`); opcodes are not, and that
asymmetry is what made the mistake easy.

## What is actually true, from a stack

Attaching `gdb` to the blocked process on the board:

```
Thread 2 "vn_wsi[0,0]":
  clock_nanosleep ()
  ... libvulkan_virtio.so ...              <- Venus WSI thread, in vn_relax
Thread 1 "vkgears":
  pthread_mutex_lock ()
  ... libvulkan_virtio.so ...
  main () at vkgears.c:1528
```

**Venus's WSI thread is sleeping in its back-off holding a mutex, and the application's main thread is
blocked acquiring that same mutex.** The WSI thread is waiting for something from the relay that never
arrives. The process is `S (sleeping)`, not spinning, and it never exits on its own.

## What it is not — four hypotheses killed

| hypothesis | test | result |
|---|---|---|
| We drop the releases | S's event witness (`RAYLAND_S_EVENT_LOG`) | **S's compositor never sends any** — only setup events |
| It is COSMIC-specific | ran against headless weston too | **fails identically** |
| Today's keymap work broke it | re-ran with the pre-keymap binaries | **fails identically — no regression** |
| The chroot's patched `vkgears` is at fault | ran stock `/usr/bin/vkgears.riscv64-linux-gnu` | **fails identically** |

And `vkcube` on the **identical** path — same C, same S, same compositor, same board — runs fine and
receives ~1,300 buffer releases. So this is specific to what `vkgears` asks Venus for, not to the
relay's event path.

One difference worth noting for whoever picks this up: against **COSMIC** vkgears gets far enough to
build its four `wl_buffer`s and then stops; against **headless weston** it never creates them at all.
Same hang, different depth.

## The recorded claim this contradicts

`CLAUDE.md` records vkgears running end to end — *"345 attaches, 345 frame callbacks, 10–13 fps, zero
panics"* (2026-08-30). That result was **apollo → dop561**, an x86_64 C. Everything here is
**milkv → dop561**, riscv64. The claim is not refuted; its scope is narrower than the sentence reads,
and this is the machine where it does not hold.

## Where to look next

The stack points inside `libvulkan_virtio.so`'s WSI path, not at Wayland. The instruments to point at
it already exist: `RAYLAND_S_STAGES` and `RAYLAND_C1_RELAXSTAT` will show whether the ring is moving
at all while the WSI thread sleeps, which separates "the relay never answered" from "the relay
answered and Venus did not notice". That is the next measurement, and it needs no new code.
