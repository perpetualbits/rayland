# The bind-failure abort — reproduced, and located — 2026-08-30

## What it is

Start `rayland-s` while another instance holds its listen port. It prints the right error and then,
**about 7–10% of the time, aborts instead of exiting 1**:

```
Error: binding S's listen address 0.0.0.0:9442 (set RAYLAND_C1_S_LISTEN to change it)
Caused by:
    Address already in use (os error 98)
rayland-s: ../src/dispatch_common.c:872: epoxy_get_proc_address:
    Assertion `0 && "Couldn't find current GLX or EGL context.\n"' failed.

Thread 2 "rayland-s-engin" received signal SIGABRT, Aborted.
```

## Rates measured

| Who | Configuration | Result |
|---|---|---|
| solsim session | `NO_PRESENT=1`, 15 runs | 1 abnormal |
| solsim session | no `NO_PRESENT`, 15 runs | 2 abnormal (both 134) |
| **this session** | `NO_PRESENT=1`, **30 runs** | **2 abnormal (both 134)** |

**~7–10%.** Signature is **SIGABRT (134)**; a single SIGSEGV (139) was seen once by the solsim session
and has not been re-provoked — treat 134 as the reliable one.

`RAYLAND_C1_NO_PRESENT` is **not** the discriminator (it crashes with and without), and neither is
`WAYLAND_DISPLAY`.

## Where it is

**`rayland-engine`'s actor thread, tearing the engine down on the startup-failure path.**

`crates/rayland-engine/src/actor.rs`'s own module docs already name this exact abort, as the reason
the actor exists: virglrenderer's EGL/surfaceless winsys binds its context to **whichever thread was
current when `virgl_renderer_init` ran**, and any GL call from another thread hits
`epoxy_get_proc_address`'s assertion — *"not a recoverable `EngineError`, it is a `SIGABRT` that takes
the whole process down."* `spawn_engine` therefore constructs, uses **and drops** the engine on one
dedicated thread.

That invariant holds here — the abort is *on* `rayland-s-engin` — so this is not the thread-affinity
bug the actor was built to prevent. It is the same libepoxy assertion reached a different way: when
the listen bind fails, `main` returns `Err` and the process begins exiting **while the actor thread is
still inside `virgl_renderer_cleanup`**. A ~7% rate is the signature of a destructor racing a
still-running thread, not a deterministic ordering bug.

Related but **not** the same as the ~21% teardown SIGABRT fixed on 2026-07-27 (`a457177`), which
covered the *session-end* path. The *startup-failure* path evidently was not covered.

## How to reproduce

From a shell with a `rayland-s` already holding the port:

```sh
for i in $(seq 1 30); do
  RAYLAND_C1_S_LISTEN=0.0.0.0:9440 ./target/release/rayland-s >/dev/null 2>&1 </dev/null
  rc=$?; [ $rc -ne 1 ] && echo "abnormal exit $rc on run $i"
done
```

**Run it in the FOREGROUND.** Under `setsid … &` the shell reaps `setsid`, which forks and returns 0,
so exit codes are meaningless and the crash is invisible. That trap is how this was nearly dismissed.

Under gdb it reproduced on the first attempt:
`gdb -q -batch -ex run -ex 'thread apply all bt' --args ./target/release/rayland-s`.

## A methodological note worth keeping

This session first reported the defect as **not reproduced**, on the strength of **four** single-shot
runs. Against a ~7% race, four runs miss it about **75%** of the time. That is the same error this
project has already recorded twice — *one run is one run, not a rate* — committed by the person who
wrote that sentence. The correction came from a peer session that ran 30.
