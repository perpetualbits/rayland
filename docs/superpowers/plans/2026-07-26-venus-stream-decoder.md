# Venus Command-Stream Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Rayland the ability to walk a whole Venus command stream — not just name its first command — by borrowing Mesa's own generated protocol decoder behind a one-function boundary.

**Architecture:** A new crate `rayland-venus-proto` vendors Mesa's `venus-protocol` headers and compiles them against a **replacement** `vkr_cs.h` that this crate writes itself (the generated headers document exactly which names they need). A small C shim drives the generated per-command decoders over a caller-supplied byte slice and reports how far the cursor advanced. Rust exposes exactly one safe function, `command_len`. All walking, error taxonomy and reporting stay in `rayland-vtest`'s existing `venus_ring::decode`.

**Tech Stack:** Rust, C (C99), the `cc` build crate, Mesa's generated `venus-protocol` headers (vendored).

**Spec:** `docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md`. Read it first — particularly "What this is not".

## Global Constraints

- **Language: Rust for all code Rayland writes.** The C here is a *borrowed* artifact plus the minimum glue to drive it, exactly as `rayland-engine` borrows `libvirglrenderer`. (CLAUDE.md locked decision.)
- **Build target (MANDATORY):** prefix EVERY cargo invocation with `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target`. The default target dir is a small tmpfs; filling it makes the linker die with a bare `SIGBUS` / `collect2: ld terminated with signal 7`. If you see that, it is disk, not your diff.
- **MSRV floor 1.85:** no let-chains or newer syntax.
- **Doc-comment on every function, type, trait, module and method** (`///` or `//!`), covering inputs, outputs, failure modes and domain pitfalls; **intent comment on every non-trivial line** — the *why*, never a restatement of the syntax. This applies to the C as well: the shim is read by the same reviewer.
- **Code and comments must always agree.** A stale comment is a bug, fixed in the same edit.
- **THE BINDING INVARIANT — diagnostic and structural only.** This decoder may **never** make a correctness decision: not what to relay, not when, not which blobs a delta reads, not whether a relay is safe. (c)1 spec §7 relays the ring as opaque bytes precisely so a decoding bug cannot become a corruption bug. Task 5 enforces this with a test; do not weaken it.
- **`rayland-c` must never link a GPU stack.** This crate is protocol-only — no driver, no device, no `libvirglrenderer` — but `tests/no_gpu_linkage.rs` must be **re-run and re-read**, not assumed.
- **No network, no GPU, no filesystem in the shim.** It is a pure function of a byte slice.

**Vendor source (read-only, do not modify):**
`/tmp/claude-1000/-home-roland-git-rayland/b5e60caa-3946-4f2d-9417-43acbd1dab44/scratchpad/virglrenderer/src/venus/venus-protocol/` — virglrenderer **1.2.0**, which is the version linked on this machine (`pkg-config --modversion virglrenderer` → `1.2.0`). If that scratch directory is gone, re-fetch virglrenderer 1.2.0 and use its `src/venus/venus-protocol/`.

---

### Task 1: The crate, the vendored headers, and a compiling shim skeleton

Create the crate, vendor Mesa's protocol headers, and write the replacement `vkr_cs.h` that satisfies the contract those headers declare. The deliverable is *it compiles* — no decoding yet.

**Files:**
- Create: `crates/rayland-venus-proto/Cargo.toml`
- Create: `crates/rayland-venus-proto/build.rs`
- Create: `crates/rayland-venus-proto/csrc/vkr_cs.h`
- Create: `crates/rayland-venus-proto/csrc/shim.c`
- Create: `crates/rayland-venus-proto/src/lib.rs`
- Create: `crates/rayland-venus-proto/vendor/MESA_VERSION`
- Create: `crates/rayland-venus-proto/vendor/venus-protocol/…` (copied)
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces: a crate that builds. No public API yet beyond a placeholder.

- [ ] **Step 1: Vendor the headers and record the version**

```bash
mkdir -p crates/rayland-venus-proto/vendor
cp -r /tmp/claude-1000/-home-roland-git-rayland/b5e60caa-3946-4f2d-9417-43acbd1dab44/scratchpad/virglrenderer/src/venus/venus-protocol \
      crates/rayland-venus-proto/vendor/venus-protocol
# The generated headers include their own "vkr_cs.h"; ours must win, so remove nothing here —
# the include path in build.rs puts csrc/ first.
printf 'virglrenderer 1.2.0\nvenus-protocol headers as generated in that release.\nSource: src/venus/venus-protocol/\n' \
  > crates/rayland-venus-proto/vendor/MESA_VERSION
ls crates/rayland-venus-proto/vendor/venus-protocol/vn_protocol_renderer_cs.h
```

- [ ] **Step 2: Write `Cargo.toml`**

```toml
# The Venus command-stream decoder: Mesa's own generated protocol, borrowed.
#
# This crate exists so Rayland can answer "where does this command end" about a real Venus stream.
# It is DIAGNOSTIC AND STRUCTURAL ONLY — see the design spec's "What this is not". It links no GPU
# driver, opens no device, and touches no network: the vendored headers are protocol definitions.
[package]
name = "rayland-venus-proto"
version = "0.0.1"
edition = "2024"
rust-version = "1.85"
license = "LGPL-3.0-or-later"
description = "Framing for Venus command streams, using Mesa's own generated protocol decoders"
repository = "https://github.com/perpetualbits/rayland"
publish = false

[build-dependencies]
# Compiles the shim and the vendored headers. A build-host requirement only; nothing links at runtime
# beyond the shim's own object code.
cc = "1"
```

- [ ] **Step 3: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/rayland-venus-proto"` to `members`, keeping the list's existing order and formatting.

- [ ] **Step 4: Write the replacement `csrc/vkr_cs.h`**

The generated `vn_protocol_renderer_cs.h` `#include "vkr_cs.h"` and then casts `vn_cs_*` to `vkr_cs_*`. Providing our own is what keeps virglrenderer (and Mesa's util library) out of the build entirely.

```c
/*
 * A minimal replacement for virglrenderer's `vkr_cs.h`.
 *
 * WHY THIS FILE EXISTS
 * Mesa's generated `vn_protocol_renderer_cs.h` declares, in its own header comment, exactly which
 * types and functions it expects a host to provide, and then `#include "vkr_cs.h"` to get them.
 * virglrenderer's version of that header pulls in `vkr_common.h`, and from there Mesa's util
 * library (hash tables, threads, os_file) — a dependency chain this crate has no use for. Since the
 * contract is small and documented, we satisfy it ourselves and the generated decoders compile with
 * nothing behind them but this file.
 *
 * WHAT THIS IS FOR, AND WHAT IT IS NOT
 * These decoders are driven ONLY to learn how many bytes a command occupies. Nothing here executes a
 * Vulkan command, resolves a real object, or writes anything anywhere. See the design spec.
 */
#ifndef RAYLAND_VKR_CS_H
#define RAYLAND_VKR_CS_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "vulkan_core.h"

/* Venus object ids are 64-bit handles chosen by the client. We never resolve one. */
typedef uint64_t vkr_object_id;

/*
 * The encoder is required by the contract but never driven: this crate decodes only. It exists as a
 * complete type so the generated headers compile, and its write is a no-op that would be a loud bug
 * if it were ever reached.
 */
struct vkr_cs_encoder {
   int unused;
};

static inline bool
vkr_cs_encoder_acquire(struct vkr_cs_encoder *enc)
{
   (void)enc;
   return true;
}

static inline void
vkr_cs_encoder_release(struct vkr_cs_encoder *enc)
{
   (void)enc;
}

static inline void
vkr_cs_encoder_write(struct vkr_cs_encoder *enc, size_t size, const void *val, size_t val_size)
{
   /* Unreachable by construction: this crate never encodes. Deliberately does nothing rather than
    * assert, because an assert here would turn a hypothetical into a crash in a diagnostic. */
   (void)enc;
   (void)size;
   (void)val;
   (void)val_size;
}

/*
 * The decoder: a cursor over the caller's bytes, a bump allocator for the arrays the generated
 * `_args_temp` decoders allocate, and a sticky fatal flag.
 *
 * `fatal` is sticky and is the ONLY error channel Mesa's decoders have — they set it and return
 * rather than propagating. Reading it after a decode is how the shim learns the command did not fit.
 */
struct vkr_cs_decoder {
   const uint8_t *cur;   /* next byte to read */
   const uint8_t *end;   /* one past the last readable byte */
   bool fatal;           /* set when a read would pass `end`; never cleared except by reset */
   uint8_t *temp;        /* bump-allocated scratch for decoded arrays */
   size_t temp_size;     /* capacity of `temp` */
   size_t temp_used;     /* bytes handed out so far */
};

static inline void
vkr_cs_decoder_set_fatal(const struct vkr_cs_decoder *dec)
{
   /* The generated code passes a const pointer; the flag is genuinely mutable state, and casting it
    * away here is what virglrenderer's own implementation does for the same reason. */
   ((struct vkr_cs_decoder *)dec)->fatal = true;
}

static inline bool
vkr_cs_decoder_get_fatal(const struct vkr_cs_decoder *dec)
{
   return dec->fatal;
}

/*
 * Object lookup: ALWAYS NULL, and that is the whole feasibility argument of this crate.
 *
 * The generated decoder reads the 8-byte id and then calls this to interpret it
 * (`vn_decode_VkDevice_lookup`). **How far the cursor advanced does not depend on the result**, so a
 * null lookup produces framing identical to a live renderer's. Validation of a null handle happens
 * in the *dispatch* functions, which this crate never calls.
 */
static inline void *
vkr_cs_decoder_lookup_object(const struct vkr_cs_decoder *dec, vkr_object_id id, VkObjectType type)
{
   (void)dec;
   (void)id;
   (void)type;
   return NULL;
}

static inline void
vkr_cs_decoder_reset_temp_pool(struct vkr_cs_decoder *dec)
{
   dec->temp_used = 0;
}

/*
 * Bump-allocate scratch for a decoded array.
 *
 * Returning NULL on exhaustion is safe: the generated decoders check the pointer and set fatal,
 * which the shim reports as a fault rather than a length. Alignment is rounded to 8, which is the
 * strictest the protocol's decoded types need.
 */
static inline void *
vkr_cs_decoder_alloc_temp(struct vkr_cs_decoder *dec, size_t size)
{
   const size_t aligned = (dec->temp_used + 7u) & ~(size_t)7u;
   if (aligned > dec->temp_size || size > dec->temp_size - aligned) {
      vkr_cs_decoder_set_fatal(dec);
      return NULL;
   }
   void *out = dec->temp + aligned;
   dec->temp_used = aligned + size;
   return out;
}

/*
 * Read `size` bytes into `val` (of which `val_size` are wanted), advancing the cursor.
 *
 * The bounds check is the mechanism that makes a truncated stream visible: overrunning sets fatal
 * and leaves the cursor alone, exactly as virglrenderer's decoder does — which is why a CS error and
 * this crate's fault mean the same thing.
 */
static inline void
vkr_cs_decoder_read(struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      vkr_cs_decoder_set_fatal(dec);
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->cur, val_size < size ? val_size : size);
   dec->cur += size;
}

/* As `read`, but without advancing — used where the protocol inspects a value before consuming it. */
static inline void
vkr_cs_decoder_peek(const struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      vkr_cs_decoder_set_fatal(dec);
      memset(val, 0, val_size);
      return;
   }
   memcpy(val, dec->cur, val_size < size ? val_size : size);
}

#endif /* RAYLAND_VKR_CS_H */
```

- [ ] **Step 5: Write a placeholder `csrc/shim.c`**

```c
/*
 * The shim: drives Mesa's generated per-command decoders to learn a command's length.
 *
 * Task 1 establishes only that the vendored protocol compiles against our `vkr_cs.h`. The decoding
 * entry point arrives in Task 2.
 */
#include "vkr_cs.h"

#include "vn_protocol_renderer_defines.h"
#include "vn_protocol_renderer_types.h"
#include "vn_protocol_renderer_handles.h"

/* Proves the translation unit links and the vendored headers are reachable. Replaced in Task 2. */
int
rayland_venus_proto_selftest(void)
{
   return (int)VK_COMMAND_TYPE_vkGetFenceStatus_EXT;
}
```

- [ ] **Step 6: Write `build.rs`**

```rust
//! Compiles the shim against the vendored `venus-protocol` headers.
//!
//! # Why the include order matters
//! The generated headers `#include "vkr_cs.h"`, and this crate supplies its own replacement for that
//! file (see `csrc/vkr_cs.h` for why). `csrc/` is therefore placed **before** the vendored directory
//! on the include path, so ours wins. Reversing these two lines would pull in virglrenderer's header
//! and, through it, Mesa's util library — which is exactly what this crate exists to avoid.
fn main() {
    // Rebuild when anything we compile changes; without these, a header edit is silently ignored.
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rerun-if-changed=vendor/venus-protocol");

    cc::Build::new()
        .file("csrc/shim.c")
        .include("csrc")
        .include("vendor/venus-protocol")
        // The vendored headers are generated code and are not ours to tidy; their warnings would
        // drown the shim's own. Rayland's C is only `csrc/`.
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers")
        .std("c99")
        .compile("rayland_venus_proto_shim");
}
```

- [ ] **Step 7: Write a placeholder `src/lib.rs`**

```rust
//! **Framing for Venus command streams** — how long is the command at the start of these bytes?
//!
//! # Why this crate exists
//! Rayland relays Venus command streams byte-exactly and, until this crate, could not read them:
//! `rayland_vtest::venus_ring::decode::encoded_size` can express only *fixed* encodings, so a walk
//! halts at the application's first real command. Most Vulkan commands are variable-length — arrays,
//! `pNext` chains, optional pointers — so no table of sizes can ever walk a real stream.
//!
//! # Why it borrows rather than reimplements
//! Mesa already generates a complete, exact decoder for its own protocol. Reimplementing it would
//! create a second source of truth for a format Rayland does not own, and when the two diverge the
//! symptom is a decoder confidently reporting the wrong thing. This mirrors `CLAUDE.md`'s locked
//! decision to reuse the Venus/virglrenderer engine rather than write one.
//!
//! # THE BINDING CONSTRAINT
//! **This crate is diagnostic and structural only.** It may report, log, measure and assert. It may
//! never decide what to relay, when to relay it, or which blobs a delta reads. (c)1 spec §7 relays
//! the ring as opaque bytes precisely so that a decoding bug cannot become a corruption bug, and
//! that reasoning is untouched here.
//!
//! See `docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md`.

// The decoding entry point arrives in Task 2; Task 1 proves the vendored protocol compiles.
unsafe extern "C" {
    /// Links the shim's self-test, proving the vendored headers compiled and linked.
    fn rayland_venus_proto_selftest() -> core::ffi::c_int;
}

/// The `VkCommandTypeEXT` of `vkGetFenceStatus`, as reported by the compiled shim.
///
/// Exists only so Task 1 has something testable: it proves the vendored protocol headers are on the
/// include path, compiled, and linked. Removed in Task 2 when the real entry point lands.
///
/// # Failure modes
/// None; the shim's implementation is a constant.
pub fn selftest_command_type() -> i32 {
    // SAFETY: the shim function takes no arguments, touches no memory, and returns a constant.
    unsafe { rayland_venus_proto_selftest() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored protocol compiles, links, and reports the value Mesa's headers define — which is
    /// the whole of Task 1's deliverable. `38` is `VK_COMMAND_TYPE_vkGetFenceStatus_EXT`, the command
    /// vkcube polls while waiting for the submit that motivated this crate.
    #[test]
    fn the_vendored_protocol_compiles_and_links() {
        assert_eq!(selftest_command_type(), 38);
    }
}
```

- [ ] **Step 8: Build and run the test**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-venus-proto 2>&1 | tail -15`
Expected: PASS, `the_vendored_protocol_compiles_and_links`. If the compile fails on a missing header, check `build.rs`'s include order — `csrc` must come first.

- [ ] **Step 9: Commit**

```bash
git add crates/rayland-venus-proto Cargo.toml
git commit -m "venus-proto: vendor Mesa's venus-protocol and compile it against our own vkr_cs.h"
```

---

### Task 2: The generated dispatch switch and `command_len`

Generate the `cmd_type → decode-args` switch from the vendored headers, and use it to implement the shim's one real entry point.

**Files:**
- Create: `crates/rayland-venus-proto/tools/gen_switch.py`
- Create: `crates/rayland-venus-proto/csrc/decode_switch.inc` (generated, committed)
- Modify: `crates/rayland-venus-proto/csrc/shim.c`

**Interfaces:**
- Produces: `int rayland_venus_command_len(const uint8_t *bytes, size_t len, uint32_t *out_cmd_type, size_t *out_len)` — returns 0 on success, non-zero fault code otherwise.

- [ ] **Step 1: Write the switch generator**

```python
#!/usr/bin/env python3
"""Emit the command-type -> decode-args switch used by the shim.

WHY THIS IS GENERATED
The switch has one case per Venus command (~331 of them) and must track Mesa exactly. Typing it by
hand would guarantee drift. This is deliberately a *symbol enumerator*, not a C parser: it only needs
to find which `vn_decode_<name>_args_temp` functions exist and what command-type enumerator each
corresponds to. That is why it is robust where parsing C types would not be.

Run from the crate root:  python3 tools/gen_switch.py > csrc/decode_switch.inc
"""
import pathlib
import re
import sys

VENDOR = pathlib.Path("vendor/venus-protocol")

# Every generated decoder is named `vn_decode_<Command>_args_temp`.
DECODER = re.compile(r"\bvn_decode_(vk[A-Za-z0-9_]+)_args_temp\s*\(")
# Every command type is `VK_COMMAND_TYPE_<Command>_EXT = <n>,`.
CMDTYPE = re.compile(r"\bVK_COMMAND_TYPE_(vk[A-Za-z0-9_]+)_EXT\s*=\s*(\d+)")

def main() -> int:
    defines = (VENDOR / "vn_protocol_renderer_defines.h").read_text()
    # command name -> numeric type, from the one header that defines them all.
    types = {m.group(1): int(m.group(2)) for m in CMDTYPE.finditer(defines)}

    decoders = set()
    for header in sorted(VENDOR.glob("vn_protocol_renderer_*.h")):
        decoders.update(m.group(1) for m in DECODER.finditer(header.read_text()))

    # Only commands that have BOTH a type and a decoder can be walked. A command with a type but no
    # decoder is not an error — it is simply one this switch reports as unknown.
    both = sorted(set(types) & decoders, key=lambda n: types[n])

    print("/* GENERATED by tools/gen_switch.py — do not edit. */")
    print("/* Regenerate after updating vendor/venus-protocol; see vendor/MESA_VERSION. */")
    print(f"/* {len(both)} commands with both a command type and a generated decoder. */")
    print("switch (cmd_type) {")
    for name in both:
        print(f"case {types[name]}: {{")
        print(f"    struct vn_command_{name} args;")
        print(f"    vn_decode_{name}_args_temp(&dec_public, &args);")
        print("    break;")
        print("}")
    print("default:")
    print("    /* A command this Mesa does not generate a decoder for. Not an error: the caller")
    print("     * reports it as an unknown type and stops, exactly as the old size table did. */")
    print("    return RAYLAND_VENUS_FAULT_UNKNOWN_COMMAND;")
    print("}")
    return 0

if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 2: Generate the switch and eyeball it**

```bash
cd crates/rayland-venus-proto
python3 tools/gen_switch.py > csrc/decode_switch.inc
head -8 csrc/decode_switch.inc
grep -c "^case " csrc/decode_switch.inc
cd ../..
```

Expected: a `case 38:` among them (`vkGetFenceStatus`), and a case count in the hundreds. If the count is 0, the regexes did not match — check that `vendor/venus-protocol/vn_protocol_renderer_defines.h` exists.

- [ ] **Step 3: Rewrite `csrc/shim.c` with the real entry point**

```c
/*
 * The shim: drives Mesa's generated per-command decoders to learn a command's length.
 *
 * # What it does
 * Given bytes positioned at the start of a Venus command, it decodes that command's arguments with
 * Mesa's own generated decoder and reports how far the cursor advanced. Nothing is executed, no
 * object is resolved, and no result is kept — only the distance.
 *
 * # What it is not
 * DIAGNOSTIC AND STRUCTURAL ONLY. See the design spec. A wrong answer here must never be able to
 * corrupt a frame, which is why nothing on the relay path may call it.
 */
#include "vkr_cs.h"

#include "vn_protocol_renderer_defines.h"
#include "vn_protocol_renderer_types.h"
#include "vn_protocol_renderer_handles.h"
#include "vn_protocol_renderer_structs.h"
#include "vn_protocol_renderer_util.h"
/* Every per-command decoder lives in one of these; the aggregate header pulls them all in. */
#include "vn_protocol_renderer.h"

/* Fault codes. 0 means success; these are returned as-is to Rust and mapped to a typed error. */
#define RAYLAND_VENUS_FAULT_TRUNCATED 1        /* the stream ended inside the command */
#define RAYLAND_VENUS_FAULT_UNKNOWN_COMMAND 2  /* no generated decoder for this command type */
#define RAYLAND_VENUS_FAULT_BAD_ARGS 3         /* caller passed a null or impossible slice */

/* Scratch for decoded arrays. Sized generously against one command, never across commands: the
 * pool is reset per call, so nothing here outlives a single `rayland_venus_command_len`. */
#define RAYLAND_VENUS_TEMP_BYTES (1u << 20)

/*
 * How many bytes does the command at the start of `bytes` occupy?
 *
 * Inputs:  `bytes`/`len` — a slice positioned at a command's `[type][flags]` prologue.
 * Outputs: `*out_cmd_type` — the decoded command type, always written when the prologue fits.
 *          `*out_len`      — the command's length in bytes, written only on success.
 * Returns: 0 on success, else one of the fault codes above.
 *
 * Total and side-effect-free: it reads the slice, writes only through its out-params, and keeps no
 * state between calls.
 */
int
rayland_venus_command_len(const uint8_t *bytes, size_t len, uint32_t *out_cmd_type, size_t *out_len)
{
   if (!bytes || !out_cmd_type || !out_len)
      return RAYLAND_VENUS_FAULT_BAD_ARGS;

   /* The temp pool is a local: one command's arrays cannot outlive this frame, and a static would
    * make the function non-reentrant for no gain. */
   static _Thread_local uint8_t temp[RAYLAND_VENUS_TEMP_BYTES];

   struct vkr_cs_decoder dec = {
      .cur = bytes,
      .end = bytes + len,
      .fatal = false,
      .temp = temp,
      .temp_size = sizeof(temp),
      .temp_used = 0,
   };
   /* The generated code takes the opaque `struct vn_cs_decoder *`; `vn_protocol_renderer_cs.h`
    * casts it straight back to ours, which is the contract that header documents. */
   struct vn_cs_decoder *dec_public = (struct vn_cs_decoder *)&dec;

   /* The prologue: type then flags, exactly as `vn_dispatch_command` reads them. */
   VkCommandTypeEXT cmd_type;
   VkCommandFlagsEXT cmd_flags;
   vn_decode_VkCommandTypeEXT(dec_public, &cmd_type);
   vn_decode_VkFlags(dec_public, &cmd_flags);
   if (dec.fatal)
      return RAYLAND_VENUS_FAULT_TRUNCATED;
   *out_cmd_type = (uint32_t)cmd_type;

#include "decode_switch.inc"

   if (dec.fatal)
      return RAYLAND_VENUS_FAULT_TRUNCATED;

   *out_len = (size_t)(dec.cur - bytes);
   return 0;
}
```

Note the generated switch names its decoder argument `dec_public`; that is why the local above uses exactly that name.

- [ ] **Step 4: Build**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo build -p rayland-venus-proto 2>&1 | tail -20`
Expected: compiles. If a generated decoder is missing a struct type, add the header that defines it to the `#include` list above — `vn_protocol_renderer.h` should cover it.

- [ ] **Step 5: Commit**

```bash
git add crates/rayland-venus-proto/tools crates/rayland-venus-proto/csrc
git commit -m "venus-proto: generated decode switch and the command_len shim entry point"
```

---

### Task 3: The safe Rust wrapper

Wrap the shim in one safe function with a typed error, confining `unsafe` to a single module.

**Files:**
- Modify: `crates/rayland-venus-proto/src/lib.rs`

**Interfaces:**
- Consumes: `rayland_venus_command_len` (Task 2).
- Produces:
  - `pub fn command_len(stream: &[u8]) -> Result<Command, DecodeFault>`
  - `pub struct Command { pub command_type: u32, pub len: usize }`
  - `pub enum DecodeFault { Truncated, UnknownCommand { command_type: u32 }, BadArgs }`

- [ ] **Step 1: Write the failing tests**

Replace the `#[cfg(test)] mod tests` in `src/lib.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// `vkGetFenceStatus` is `[type=38][flags][device u64][fence u64]` — 24 bytes — and it is the
    /// command vkcube polls while waiting for the submit that motivated this crate. Decoding it
    /// proves the borrowed decoders run and that framing comes out of them.
    #[test]
    fn a_fixed_size_command_reports_its_real_length() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&38u32.to_le_bytes()); // command type
        stream.extend_from_slice(&0u32.to_le_bytes()); // flags
        stream.extend_from_slice(&0x1111_1111_1111_1111u64.to_le_bytes()); // device handle
        stream.extend_from_slice(&0x2222_2222_2222_2222u64.to_le_bytes()); // fence handle
        let cmd = command_len(&stream).expect("a decodable command");
        assert_eq!(cmd.command_type, 38);
        assert_eq!(cmd.len, 24);
    }

    /// A stream cut inside a command must be a typed fault, never a panic and never a plausible
    /// wrong length — a wrong length is how a walker desynchronizes and invents commands.
    #[test]
    fn a_truncated_command_is_a_fault_not_a_guess() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&38u32.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes()); // only one of the two handles
        assert_eq!(command_len(&stream), Err(DecodeFault::Truncated));
    }

    /// Too short even for the prologue. The prologue is where every walk begins, so this is the
    /// boundary a caller hits at the end of every stream.
    #[test]
    fn a_slice_too_short_for_the_prologue_is_truncated() {
        assert_eq!(command_len(&[0u8; 3]), Err(DecodeFault::Truncated));
    }

    /// A command type Mesa generates no decoder for is reported *with its type*, so a caller can say
    /// which command stopped it — the single most useful fact this crate produces when it fails.
    #[test]
    fn an_unknown_command_type_reports_which_one() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&9_999u32.to_le_bytes()); // past the end of the enum
        stream.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            command_len(&stream),
            Err(DecodeFault::UnknownCommand { command_type: 9_999 })
        );
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-venus-proto 2>&1 | tail -12`
Expected: FAIL to compile — `command_len`, `Command` and `DecodeFault` do not exist yet.

- [ ] **Step 3: Implement the wrapper**

Replace the placeholder `extern` block and `selftest_command_type` in `src/lib.rs` with:

```rust
unsafe extern "C" {
    /// The shim's one entry point. See `csrc/shim.c` for the contract; the wrapper below is the only
    /// caller and is what makes that contract safe to rely on.
    fn rayland_venus_command_len(
        bytes: *const u8,
        len: usize,
        out_cmd_type: *mut u32,
        out_len: *mut usize,
    ) -> core::ffi::c_int;
}

/// One decoded command's framing: what it is, and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    /// The `VkCommandTypeEXT` read from the command's prologue.
    pub command_type: u32,
    /// The command's total length in bytes, prologue included — the stride to the next command.
    pub len: usize,
}

/// Why a command could not be framed.
///
/// Every variant is a refusal to guess. A walker that receives one of these must stop, because the
/// position of every following command is unknown — which is precisely the desynchronisation the old
/// size table's conservatism was protecting against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFault {
    /// The stream ended inside the command. **This is the same condition virglrenderer reports as a
    /// "CS error"** — its decoder and this one share the mechanism, so the two agree by construction.
    Truncated,
    /// This Mesa generates no decoder for that command type, so its length is unknowable here.
    UnknownCommand {
        /// The type read from the prologue, so a caller can name what stopped it.
        command_type: u32,
    },
    /// The shim rejected its arguments. Unreachable from this wrapper, which always passes a valid
    /// slice and valid out-params; present because the C contract admits it.
    BadArgs,
}

/// How many bytes does the command at the start of `stream` occupy, and which command is it?
///
/// # Inputs / outputs
/// - `stream`: bytes positioned at a command's `[type][flags]` prologue. Not consumed; nothing is
///   retained. An empty slice is [`DecodeFault::Truncated`].
/// - Returns the command's type and length, or a typed fault.
///
/// # Failure modes
/// See [`DecodeFault`]. Cannot panic: every fault the borrowed decoders can raise becomes a variant.
///
/// # Pitfall: this reports framing, not meaning
/// The command's arguments are decoded and then discarded. Object handles are read but never
/// resolved — the shim's lookup returns null, which cannot change how many bytes were consumed. Do
/// not read anything into the fact that a command decoded successfully: it means the bytes were
/// well-formed, not that the command would execute.
///
/// # Pitfall: never make a correctness decision from this
/// (c)1 spec §7 relays the ring as opaque bytes so that a decoding bug cannot become a corruption
/// bug. This function is for reporting, measuring and testing. See the crate docs.
pub fn command_len(stream: &[u8]) -> Result<Command, DecodeFault> {
    let mut command_type: u32 = 0;
    let mut len: usize = 0;
    // SAFETY: `stream` is a valid slice of `stream.len()` readable bytes; the two out-params are
    // valid, writable locals. The shim reads only within the slice, writes only through the
    // out-params, allocates nothing that outlives the call, and keeps no state between calls.
    let rc = unsafe {
        rayland_venus_command_len(stream.as_ptr(), stream.len(), &mut command_type, &mut len)
    };
    match rc {
        0 => Ok(Command { command_type, len }),
        1 => Err(DecodeFault::Truncated),
        2 => Err(DecodeFault::UnknownCommand { command_type }),
        // Any other code is a shim contract violation. Reported as `BadArgs` rather than panicking:
        // a diagnostic that aborts the process is worse than one that says it does not know.
        _ => Err(DecodeFault::BadArgs),
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-venus-proto 2>&1 | tail -12`
Expected: PASS — all four tests.

If `a_fixed_size_command_reports_its_real_length` reports a length other than 24, do **not** adjust the expected number: check `vn_decode_vkGetFenceStatus_args_temp` in `vendor/venus-protocol/vn_protocol_renderer_fence.h` and work out what the decoder actually reads. The test encodes the wire format the design spec describes, and a disagreement means one of the two is wrong.

- [ ] **Step 5: Teeth-check the truncation test**

Temporarily change `csrc/vkr_cs.h`'s `vkr_cs_decoder_read` bounds check from `<` to `>` (so it never sets fatal), rebuild, and confirm `a_truncated_command_is_a_fault_not_a_guess` now FAILS. Then restore it.

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-venus-proto a_truncated 2>&1 | tail -8`
Expected: FAIL while inverted; PASS after restoring.

- [ ] **Step 6: Commit**

```bash
git add crates/rayland-venus-proto/src/lib.rs
git commit -m "venus-proto: safe command_len wrapper with typed faults"
```

---

### Task 4: Walk whole streams in `venus_ring::decode`

Teach the existing walker to continue past commands the size table cannot express, and prove it on the captured fixture.

**Files:**
- Modify: `crates/rayland-vtest/Cargo.toml`
- Modify: `crates/rayland-vtest/src/venus_ring/decode.rs`

**Interfaces:**
- Consumes: `rayland_venus_proto::{command_len, Command, DecodeFault}` (Task 3).
- Produces: `decode_commands` unchanged in signature, able to reach `DecodeStop::ReachedEnd` on real streams.

- [ ] **Step 1: Add the dependency**

In `crates/rayland-vtest/Cargo.toml`, under `[dependencies]`:

```toml
# Framing for command streams whose encoding is variable-length — everything the fixed-size table
# below cannot express. DIAGNOSTIC ONLY: see that crate's docs and (c)1 spec §7.
rayland-venus-proto = { path = "../rayland-venus-proto" }
```

- [ ] **Step 2: Write the failing test**

Add to the `#[cfg(test)] mod tests` in `crates/rayland-vtest/src/venus_ring/decode.rs`:

```rust
    /// The size table and the borrowed decoder must agree wherever both can answer. They are
    /// independent — one is hand-derived from Mesa's `vn_sizeof_*`, the other is Mesa's own decoder —
    /// so agreement is evidence and disagreement means one of them is wrong.
    #[test]
    fn the_size_table_and_the_borrowed_decoder_agree() {
        // `vkNotifyRingMESA`: type, flags, ring handle, seqno, flags — the doorbell, and one of the
        // three commands the table knows.
        let mut stream = Vec::new();
        stream.extend_from_slice(&VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0xdead_beefu64.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.extend_from_slice(&0u32.to_le_bytes());

        let from_table = encoded_size(VK_COMMAND_TYPE_VK_NOTIFY_RING_MESA)
            .expect("the table knows the doorbell");
        let from_decoder = rayland_venus_proto::command_len(&stream)
            .expect("the borrowed decoder frames the doorbell")
            .len;
        assert_eq!(
            from_table, from_decoder,
            "the size table and Mesa's own decoder disagree about vkNotifyRingMESA"
        );
    }
```

- [ ] **Step 3: Run it to verify it fails**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-vtest the_size_table_and 2>&1 | tail -10`
Expected: FAIL to compile — `rayland_venus_proto` is not yet a dependency in scope, or the test is the first use of it.

- [ ] **Step 4: Extend the walker**

In `decode_commands`, replace the block that ends the walk on an unknown size:

```rust
        let Some(encoded_size) = encoded_size(command_type) else {
            return (
                commands,
                DecodeStop::UnknownCommandSize {
                    offset,
                    command_type,
                },
            );
        };
```

with:

```rust
        // The fixed-size table answers first: it is pure Rust, needs no C, and covers the three
        // commands this crate cares about most — including the doorbell. Where it cannot answer, ask
        // Mesa's own decoder, which is the only thing that can frame a variable-length command.
        // **Framing only.** Nothing decoded here may inform a relay decision; see (c)1 spec §7 and
        // `rayland_venus_proto`'s crate docs.
        let encoded_size = match encoded_size(command_type) {
            Some(size) => size,
            None => match rayland_venus_proto::command_len(&stream[offset..]) {
                Ok(command) => command.len,
                // The borrowed decoder ran out of bytes: the slice cuts through this command, which
                // is the same condition virglrenderer reports as a "CS error".
                Err(rayland_venus_proto::DecodeFault::Truncated) => {
                    return (commands, DecodeStop::Truncated { offset });
                }
                // Neither the table nor Mesa can size it. Reported exactly as before, so this arm
                // keeps its old meaning: this module ran out of knowledge, not the client out of
                // commands.
                Err(_) => {
                    return (
                        commands,
                        DecodeStop::UnknownCommandSize {
                            offset,
                            command_type,
                        },
                    );
                }
            },
        };
```

- [ ] **Step 5: Run the decode tests**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-vtest venus_ring 2>&1 | tail -15`
Expected: PASS, including the new agreement test and every pre-existing `venus_ring` test.

If a pre-existing test asserted `UnknownCommandSize` at a particular offset and now gets further, that test is asserting the **old limitation**, not a requirement. Update it to the new behaviour and say so in the commit message — do not weaken the walker to keep it green.

- [ ] **Step 6: Assert the captured fixture walks whole**

Add to the same `tests` module:

```rust
    /// **The anchor.** The captured ring is a real Venus client's stream, and its `head` counter was
    /// written by a real virglrenderer that consumed it. Walking the whole thing and arriving at
    /// exactly that byte total is two independent implementations meeting: our sizes come from
    /// Mesa's decoder, the total from virglrenderer's consumer. Before this crate, the walk stopped
    /// at the first application command.
    #[test]
    fn the_captured_ring_walks_to_its_end() {
        let stream = super::captured::CAPTURED_RING_COMMAND_STREAM;
        let (commands, stop) = decode_commands(stream);
        assert_eq!(
            stop,
            DecodeStop::ReachedEnd,
            "the walk stopped early at command {}",
            commands.len()
        );
        let walked: usize = commands.iter().map(|c| c.encoded_size).sum();
        assert_eq!(
            walked,
            stream.len(),
            "the decoded commands do not account for every byte of the capture"
        );
    }
```

- [ ] **Step 7: Run it**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-vtest the_captured_ring_walks 2>&1 | tail -12`
Expected: PASS.

If it stops early on an `UnknownCommand`, that is a real finding, not a test to relax: it means this Mesa generates no decoder for a command the capture contains. Record the command type in the commit message and leave the test asserting `ReachedEnd` only if it genuinely passes; if it does not, change the assertion to name the specific command that stopped it and open it as a known gap in `docs/DIARY.md`.

- [ ] **Step 8: Commit**

```bash
git add crates/rayland-vtest/Cargo.toml crates/rayland-vtest/src/venus_ring/decode.rs
git commit -m "vtest: walk whole command streams via the borrowed Venus decoder"
```

---

### Task 5: Guards and documentation

Enforce the binding invariant, re-verify the no-GPU guard rather than assuming it, and correct the two documents this change makes false.

**Files:**
- Create: `crates/rayland-c/tests/decoder_is_not_load_bearing.rs`
- Modify: `crates/rayland-vtest/src/lib.rs` (crate docs)
- Modify: `CLAUDE.md`
- Modify: `project-map.js`

**Interfaces:**
- Consumes: everything above. Produces: no new API.

- [ ] **Step 1: Write the invariant test**

```rust
//! **The decoder may never make a correctness decision.**
//!
//! (c)1 spec §7 relays the ring as opaque bytes precisely so that a decoding bug cannot become a
//! corruption bug. `rayland-venus-proto` exists to *report* what a stream contains; the moment
//! anything on the relay path decides something from it, a wrong answer stops being a wrong
//! diagnosis and starts being a wrong frame.
//!
//! This test is the mechanical half of that rule. The prose half is in the design spec and in the
//! decoder crate's own docs, and prose does not fail a build.
//!
//! # What it actually checks, and its honest limit
//! It asserts the relay's own modules do not name the decoder crate. It cannot prove absence of
//! influence in general — a future author could route a decision through a third module — but it
//! catches the direct, obvious version, which is the one that would happen by accident.

/// The relay path's source files: the modules that decide what crosses the wire and when.
const RELAY_PATH: &[&str] = &[
    "src/blob_sync.rs",
    "src/ring.rs",
    "src/relay_engine.rs",
    "src/link.rs",
];

#[test]
fn the_relay_path_does_not_consult_the_stream_decoder() {
    for file in RELAY_PATH {
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|e| panic!("cannot read {file}, which this guard must inspect: {e}"));
        // The crate name in any form — a `use`, a path call, or a re-export.
        assert!(
            !source.contains("rayland_venus_proto"),
            "{file} references the Venus stream decoder. That decoder is DIAGNOSTIC ONLY: it may \
             report what a stream contains, never decide what to relay. See (c)1 spec §7 and \
             docs/superpowers/specs/2026-07-26-venus-stream-decoder-design.md."
        );
    }
}
```

- [ ] **Step 2: Run it**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --test decoder_is_not_load_bearing 2>&1 | tail -8`
Expected: PASS.

- [ ] **Step 3: Teeth-check it**

Temporarily add the line `// rayland_venus_proto` to `crates/rayland-c/src/blob_sync.rs`, re-run, and confirm the test FAILS with the message above. Then remove the line.

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --test decoder_is_not_load_bearing 2>&1 | tail -8`
Expected: FAIL while the line is present; PASS after removing it.

- [ ] **Step 4: Re-verify the no-GPU guard**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test -p rayland-c --test no_gpu_linkage 2>&1 | tail -6`
Expected: PASS.

Then **read** `crates/rayland-c/tests/no_gpu_linkage.rs` and confirm what it actually asserts. It was written to assert `rayland-engine` is absent from the dependency tree. That assertion still holds and still means what it meant — but the crate it guards now pulls in C. If the test's doc comment claims the tree is "only `libc` and `thiserror`", correct it in this task; a guard that describes itself wrongly is worse than none.

- [ ] **Step 5: Correct `rayland-vtest`'s crate docs**

In `crates/rayland-vtest/src/lib.rs`, find the module-level statement that the crate has no GPU dependencies "only `libc` and `thiserror`" and replace it with the truth:

```rust
//! **Has no GPU dependencies, by construction.** It links `libc`, `thiserror`, and
//! `rayland-venus-proto` — the last of which compiles Mesa's *generated protocol headers*, which are
//! format definitions with no driver, no device and no `libvirglrenderer` behind them. Rayland's
//! **C** side speaks this protocol but must never link a GPU stack (C is the weak, possibly
//! headless, possibly RISC-V machine), and `rayland-c`'s `tests/no_gpu_linkage.rs` asserts
//! `rayland-engine` is absent from its dependency tree. **The dependency arrow points
//! `rayland-engine` → `rayland-vtest`, and must never be reversed.**
```

- [ ] **Step 6: Correct `CLAUDE.md`**

In the `crates/rayland-vtest` bullet under "Repository status and layout", the phrase "only `libc` and `thiserror`" is now false. Update it to match the crate docs above, and add a bullet for the new crate in the same list, in the same voice as its neighbours:

```markdown
- **`crates/rayland-venus-proto`** — **framing for Venus command streams**: how long is the command
  at the start of these bytes? It vendors Mesa's *generated* `venus-protocol` headers and compiles
  them against a replacement `vkr_cs.h` this crate writes itself, so the borrowed decoders run with
  no virglrenderer and no Mesa util library behind them. Byte consumption does not depend on object
  lookups, so stub lookups give framing identical to a live renderer's. **Diagnostic and structural
  only — it may never make a correctness decision** ((c)1 spec §7: the ring is relayed as opaque
  bytes precisely so a decoding bug cannot become a corruption bug), enforced by
  `rayland-c/tests/decoder_is_not_load_bearing.rs`. LGPL, `publish = false`.
```

- [ ] **Step 7: Update the project map**

In `project-map.js`, change the `venus-proto` node's `status` from `"planned"` to `"done"`, replace its "SPEC'D, NOT BUILT" opening with what was built, add a `files` array naming the crate's real paths, and set `project.updated` to the day's date. Verify it still parses:

```bash
node -e "global.window={};require('./project-map.js');console.log('nodes:',window.PROJECT_MAP.nodes.length)"
```

- [ ] **Step 8: Full workspace check**

Run: `CARGO_TARGET_DIR=/home/roland/.cache/rayland-c1-target cargo test --workspace 2>&1 | tail -12`
Expected: PASS, no failures. This includes the GPU-backed `loopback_e2e`, which must stay bit-identical — the decoder is not on that path, so a change there would mean something in this work escaped its boundary.

- [ ] **Step 9: Diary and commit**

Add a `docs/DIARY.md` entry: what was built, what the fixture walk actually showed (including any command Mesa generates no decoder for), and whether the borrowed decoder and the size table agreed. Then:

```bash
git add crates/rayland-c/tests/decoder_is_not_load_bearing.rs crates/rayland-vtest/src/lib.rs \
        CLAUDE.md project-map.js docs/DIARY.md
git commit -m "venus-proto: enforce diagnostic-only, re-verify no-GPU guard, correct the docs it falsifies"
```

---

## Self-Review

**Spec coverage:**
- Borrow Mesa's protocol rather than reimplement → Task 1 (vendor) + Task 2 (drive it). ✓
- Replacement `vkr_cs.h` supplying the documented contract → Task 1 Step 4. ✓
- Stub lookups; framing independent of resolution → Task 1 Step 4 (`vkr_cs_decoder_lookup_object`), with the argument in its doc comment. ✓
- Generated switch, ~331 cases, regenerable per Mesa version → Task 2 Steps 1–2. ✓
- One-function public surface (`command_len`) → Task 3. ✓
- Walking/errors/reporting stay in Rust in `venus_ring::decode` → Task 4. ✓
- Fixture walked whole, byte total equals the host's `head` → Task 4 Steps 6–7. ✓
- Cross-check against `encoded_size` → Task 4 Step 2. ✓
- Faults typed, never a panic or a plausible wrong answer → Task 3 Step 1 (three fault tests) + Step 5 teeth-check. ✓
- **Binding invariant enforced by a test** → Task 5 Steps 1–3. ✓
- `no_gpu_linkage` re-run *and re-read* → Task 5 Step 4. ✓
- Vendored headers with recorded Mesa version → Task 1 Step 1. ✓
- Docs corrected where this change falsifies them → Task 5 Steps 5–6. ✓

**Placeholder scan:** No TBD/TODO. Every code step carries literal code; every test step carries literal assertions, a run command and an expected result. Two steps deliberately describe a *judgement* rather than a fixed outcome — Task 4 Step 7 (what to do if the fixture stops early) and Task 5 Step 4 (read the guard, correct it if it misdescribes itself) — and both state exactly what to do in each case rather than leaving it open.

**Type consistency:** `command_len(&[u8]) -> Result<Command, DecodeFault>` is named identically in Task 3 (definition) and Task 4 (call). `Command { command_type: u32, len: usize }` matches its use as `.len` in Task 4 and `.command_type` in Task 3's tests. `DecodeFault::{Truncated, UnknownCommand { command_type }, BadArgs }` matches the C fault codes 1/2/3 in Task 2 and the match arms in Task 3. The C entry point `rayland_venus_command_len(const uint8_t*, size_t, uint32_t*, size_t*) -> int` matches the Rust `extern` declaration exactly. The generated switch's decoder variable `dec_public` matches the local declared in Task 2 Step 3. `DecodeStop::{ReachedEnd, Truncated, UnknownCommandSize}` are the existing variants, used unchanged.
