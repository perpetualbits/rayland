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
/* The command's bytes were all present; the 1 MiB scratch pool below (`RAYLAND_VENUS_TEMP_BYTES`)
 * was too small to hold what the generated decoder needed to allocate while decoding it. Distinct
 * from `_TRUNCATED` on purpose: virglrenderer's own pool cap is 1 GiB
 * (`VKR_CS_DECODER_TEMP_POOL_MAX_SIZE`), 1000x this one, so a command a real renderer decodes fine
 * could hit this here — reporting it as a truncated stream would be a wrong diagnosis, not merely an
 * imprecise one. See `struct vkr_cs_decoder::pool_exhausted` in vkr_cs.h. */
#define RAYLAND_VENUS_FAULT_TEMP_POOL_EXHAUSTED 4

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

   /* This IS a `_Thread_local static`: its bytes persist between calls on the same thread, rather
    * than being freshly stack-allocated each time. That persistence is harmless because `temp_used`
    * — the bump allocator's cursor, reset to 0 a few lines below, at the top of `dec`'s initializer —
    * is what actually delimits "this call's" allocations; no call can ever read another call's
    * leftover bytes, because nothing reads `temp` except through offsets below the current
    * `temp_used`, and those are always freshly written before they are read. `_Thread_local` (not a
    * plain `static`) is the part that is load-bearing: two threads may call
    * `rayland_venus_command_len` concurrently (`rayland-c`'s ring watcher and `rayland_venus_proto`'s
    * own tests both do), and a plain `static` would let them race over the same bytes and the same
    * `temp_used` cursor — each thread's `_Thread_local` copy makes that impossible by construction,
    * at zero cost to reentrancy. `_Alignas(8)` makes explicit the precondition
    * `vkr_cs_decoder_alloc_temp` (csrc/vkr_cs.h) trusts rather than checks: rounding `temp_used` up
    * to a multiple of 8 only yields an 8-byte-aligned *address* if `temp` itself already is one —
    * otherwise the rounding just preserves whatever misalignment the base pointer started with. This
    * is the first place anything populates `temp`, so this is where that precondition must be paid. */
   static _Thread_local _Alignas(8) uint8_t temp[RAYLAND_VENUS_TEMP_BYTES];

   struct vkr_cs_decoder dec = {
      .cur = bytes,
      .end = bytes + len,
      .fatal = false,
      .temp = temp,
      .temp_size = sizeof(temp),
      .temp_used = 0,
      .pool_exhausted = false, /* only an allocator (vkr_cs.h) ever sets this true */
   };
   /* The generated code takes the opaque `struct vn_cs_decoder *`; `vn_protocol_renderer_cs.h`
    * casts it straight back to ours, which is the contract that header documents. */
   struct vn_cs_decoder *dec_public = (struct vn_cs_decoder *)&dec;

   /* A small number of commands' generated decoders ALSO take an encoder (see the long comment on
    * `vkr_cs_encoder_get_blob_storage` in vkr_cs.h for which ones, and why): Mesa pre-sizes that
    * command's reply arena during the decode pass. We never populate `enc` with anything real —
    * this shim never replies to anything — but `decode_switch.inc` still needs a valid
    * `struct vn_cs_encoder *` to pass for those commands, or the call does not type-check. */
   struct vkr_cs_encoder enc = { .unused = 0 };
   struct vn_cs_encoder *enc_public = (struct vn_cs_encoder *)&enc;

   /* The prologue: type then flags, exactly as `vn_dispatch_command` reads them. */
   VkCommandTypeEXT cmd_type;
   VkCommandFlagsEXT cmd_flags;
   vn_decode_VkCommandTypeEXT(dec_public, &cmd_type);
   vn_decode_VkFlags(dec_public, &cmd_flags);
   /* Only a short read can fail here: the prologue never allocates from `temp`, so `pool_exhausted`
    * cannot be set yet — there is nothing for it to distinguish from at this point in the function. */
   if (dec.fatal)
      return RAYLAND_VENUS_FAULT_TRUNCATED;
   *out_cmd_type = (uint32_t)cmd_type;

#include "decode_switch.inc"

   if (dec.fatal) {
      /* Check pool exhaustion FIRST: it is the more specific fact whenever both are true (which they
       * always are here, since `vkr_cs_decoder_alloc_temp` sets both together) — see the field's doc
       * comment on `struct vkr_cs_decoder` in vkr_cs.h for why the two must never be conflated. */
      if (dec.pool_exhausted)
         return RAYLAND_VENUS_FAULT_TEMP_POOL_EXHAUSTED;
      return RAYLAND_VENUS_FAULT_TRUNCATED;
   }

   *out_len = (size_t)(dec.cur - bytes);
   return 0;
}
