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
 * Candidates for "indirect" storage are VkInstance, VkPhysicalDevice, VkDevice, VkQueue and
 * VkCommandBuffer — the five "dispatchable" Vulkan handle types (`VK_DEFINE_HANDLE` in the Vulkan
 * headers, as opposed to `VK_DEFINE_NON_DISPATCHABLE_HANDLE` for every other handle type). Whether one
 * of them actually NEEDS indirection is a **pointer-size comparison**, not a fixed yes/no per type —
 * see below.
 *
 * NOT PART OF THE DOCUMENTED CONTRACT (see the note above `vkr_object`, further down) but required to
 * compile: `vn_decode_Vk*_temp` (the handle-typed argument decoder our shim's call graph does reach)
 * calls this for every handle it decodes.
 *
 * WHY THE DISTINCTION EXISTS: a real ICD loader dereferences a dispatchable handle as a pointer to a
 * dispatch-table struct — that is the mechanism Vulkan loaders use to find a driver's function
 * pointers. A decoder fabricating an *unresolved* handle for one of these five types therefore cannot
 * use the raw wire id as the handle value directly *if the handle is narrower than the wire id* — it
 * must give the handle somewhere valid to point, hence "indirect": the caller allocates a temp-pool
 * cell and stores the wire id there, and the handle becomes a pointer to that cell. Every other
 * (non-dispatchable) handle type is always exactly as wide as the wire id (Vulkan's own
 * `VK_DEFINE_NON_DISPATCHABLE_HANDLE` macro widens it to a 64-bit-compatible representation whenever
 * pointers are narrower), so it is never indirect.
 *
 * THE ACTUAL CONDITION, matching virglrenderer's own `vkr_cs.h`, is `sizeof(VkInstance) <
 * sizeof(vkr_object_id)` — a comparison between the host's pointer width and the 64-bit wire id, not a
 * hardcoded "dispatchable handles are always indirect". On every 64-bit build (the only realistic
 * target for this crate: `sizeof(VkInstance) == sizeof(void*) == 8 == sizeof(vkr_object_id)`), that
 * comparison is **false** — so on this architecture dispatchable handles are direct too, exactly like
 * non-dispatchable ones. Writing the comparison out (rather than hardcoding `false`) is what keeps this
 * correct if this crate is ever built for a 32-bit target, where it would evaluate `true`.
 *
 * The switch below is still inferred directly from two call sites in the vendored tree, not a guess:
 * `vn_decode_VkInstance_temp` (`vn_protocol_renderer_handles.h`) allocates a temp cell before storing,
 * while `vn_decode_VkBuffer` in the same file stores straight into the handle slot — confirming that
 * only the five dispatchable types are ever candidates for the indirect branch at all. Getting the
 * candidate set wrong, or the size comparison wrong, cannot corrupt this crate's byte-count answer — no
 * wire bytes are read here, only scratch-pool bookkeeping.
 */
static inline bool
vkr_cs_handle_indirect_id(VkObjectType type)
{
   switch (type) {
   case VK_OBJECT_TYPE_INSTANCE:
   case VK_OBJECT_TYPE_PHYSICAL_DEVICE:
   case VK_OBJECT_TYPE_DEVICE:
   case VK_OBJECT_TYPE_QUEUE:
   case VK_OBJECT_TYPE_COMMAND_BUFFER:
      /* False on every 64-bit build; written as a comparison, not a hardcoded constant, so it stays
       * correct if this crate is ever built where pointers are narrower than a `vkr_object_id`. */
      return sizeof(VkInstance) < sizeof(vkr_object_id);
   default:
      return false;
   }
}

/*
 * Store a decoded wire id into a handle slot, per the indirect/direct distinction documented on
 * `vkr_cs_handle_indirect_id`, above.
 *
 * For indirect (dispatchable) types, `*handle` was already pointed at a temp-allocated cell by the
 * caller (`vn_decode_Vk*_temp`, before this is called) — the id is written THROUGH that pointer. For
 * direct types, `*handle` IS the storage: the id is written into the slot itself, reinterpreted as a
 * pointer-sized value. Neither branch touches the byte cursor; this manages only the in-memory shape
 * of a fabricated handle and has no bearing on `command_len`.
 */
static inline void
vkr_cs_handle_store_id(void **handle, vkr_object_id id, VkObjectType type)
{
   if (vkr_cs_handle_indirect_id(type)) {
      *(vkr_object_id *)(*handle) = id;
   } else {
      *handle = (void *)(uintptr_t)id;
   }
}

/*
 * The encode-side mirror of `vkr_cs_handle_store_id`: recover a wire id from a handle slot.
 *
 * DEAD CODE FOR THIS CRATE'S PURPOSES — reachable only from `vn_encode_Vk*` helpers, and this crate
 * never encodes (see `vkr_cs_encoder_write`, above). Implemented for real anyway, rather than stubbed,
 * because the correct body is the mechanical inverse of `vkr_cs_handle_store_id` and costs nothing
 * extra to get right.
 */
static inline vkr_object_id
vkr_cs_handle_load_id(const void **handle, VkObjectType type)
{
   if (vkr_cs_handle_indirect_id(type)) {
      return *(const vkr_object_id *)(*handle);
   }
   return (vkr_object_id)(uintptr_t)(*handle);
}

/*
 * Bump-allocate scratch for a decoded array.
 *
 * Returning NULL on exhaustion is safe: the generated decoders check the pointer and set fatal,
 * which the shim reports as a fault rather than a length. Alignment is rounded to 8, which is the
 * strictest the protocol's decoded types need.
 *
 * PRECONDITION THIS FUNCTION TRUSTS RATHER THAN ENFORCES: `dec->temp` itself must already be
 * 8-byte aligned. Rounding `temp_used` up to a multiple of 8 only produces an 8-byte-aligned
 * *address* if the base pointer it is added to is itself a multiple of 8 — otherwise the rounding
 * faithfully preserves whatever misalignment the base already had. Nothing in this header can check
 * that (a `struct vkr_cs_decoder` only ever sees `temp` as an opaque `uint8_t *`, with no way to ask
 * the compiler for its alignment at this point), so the caller that populates `temp` is the one that
 * must guarantee it — see the `_Alignas(8)` on the backing buffer in `shim.c`, the first and only
 * place that allocates one.
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
 *
 * ALIASING: `val` can legitimately equal `dec->cur`. `vkr_cs_decoder_get_blob_storage`, below, hands
 * back a pointer straight into the decoder's own buffer (matching virglrenderer's own
 * implementation) rather than a copy, so the generated blob-array decode ends up calling this with
 * `val == dec->cur` — a same-address "copy" that is well-defined only because we special-case it.
 * `memcpy` with overlapping (here: identical) source and destination is undefined behaviour, so the
 * `dec->cur != val` guard below — copied from virglrenderer's own `vkr_cs_decoder_read`/`_peek` for
 * exactly this reason — skips the copy entirely when it would alias.
 */
static inline void
vkr_cs_decoder_read(struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      vkr_cs_decoder_set_fatal(dec);
      memset(val, 0, val_size);
      return;
   }
   /* Skip the copy when source and destination coincide (see the aliasing note above) — copying
    * would be UB, and it would also be pointless: the bytes are already exactly where they belong. */
   if (dec->cur != val)
      memcpy(val, dec->cur, val_size < size ? val_size : size);
   dec->cur += size;
}

/* As `read`, but without advancing — used where the protocol inspects a value before consuming it.
 * Same aliasing hazard and same guard as `vkr_cs_decoder_read`, above. */
static inline void
vkr_cs_decoder_peek(const struct vkr_cs_decoder *dec, size_t size, void *val, size_t val_size)
{
   if ((size_t)(dec->end - dec->cur) < size) {
      vkr_cs_decoder_set_fatal(dec);
      memset(val, 0, val_size);
      return;
   }
   if (dec->cur != val)
      memcpy(val, dec->cur, val_size < size ? val_size : size);
}

/*
 * The four members below (`struct vkr_object`, `vkr_cs_decoder_alloc_temp_array`,
 * `vkr_cs_decoder_get_blob_storage`, `vkr_cs_encoder_get_blob_storage`) are NOT part of the contract
 * documented in `vn_protocol_renderer_cs.h`'s own header comment (the "these types/functions are
 * expected" list this file otherwise satisfies exactly). They were discovered only when this crate
 * first tried to compile against the real vendored headers: at virglrenderer 1.2.0, several generated
 * per-command decoders (e.g. `vn_decode_VkSpecializationInfo_temp`, used by pipeline creation;
 * `vn_decode_VkPushConstantsInfo_self_temp`, used by `vkCmdPushConstants2`) call
 * `vkr_cs_decoder_get_blob_storage`, and the generated `vn_replace_Vk*_handle` helpers (present in
 * every handle-typed header, even though this crate's decode-only call graph never reaches them)
 * dereference a `struct vkr_object`. Mesa's own comment simply never caught up to its generator. See
 * Task 1's report for the discovery. Task 1 left the two blob-storage functions as NULL stubs because
 * nothing in Task 1 called them; Task 2 implements both for real (see their own doc comments, below)
 * because Task 2 drives real per-command decoders, several of which carry a blob array.
 */

/*
 * A resolved Venus object, as `vkr_cs_decoder_lookup_object` would return one.
 *
 * This crate's lookup ALWAYS returns NULL (see above), so no instance of this struct is ever actually
 * constructed by anything in this crate — only the *type* is needed, so that the generated
 * `vn_replace_Vk*_handle` functions type-check. `handle.u64` mirrors virglrenderer's own field name,
 * since the generated code (`vn_protocol_renderer_handles.h`) reads exactly that member.
 */
struct vkr_object {
   union {
      uint64_t u64; /* the host-side handle value, as a raw 64-bit integer */
      void *ptr;    /* ...or as a pointer, for handle types the host represents as one */
   } handle;
};

/*
 * The array-counted sibling of `vkr_cs_decoder_alloc_temp`: allocate `count` contiguous elements of
 * `size` bytes each from the same bump pool.
 *
 * Unlike the blob-storage stubs below, this one Task 1 implements for real rather than deferring: it
 * is a mechanical composition of primitives the design spec already blessed (`vkr_cs_decoder_alloc_temp`
 * plus a multiply), not a new semantic decision. The overflow guard matters because `count` is decoded
 * from the (untrusted) stream — without it, a huge count could wrap `size * count` down to a small
 * allocation that the generated code would then decode past the end of, corrupting memory beyond this
 * crate's own arena rather than merely failing the framing question it exists to answer.
 */
static inline void *
vkr_cs_decoder_alloc_temp_array(struct vkr_cs_decoder *dec, size_t size, size_t count)
{
   if (count != 0 && size > (size_t)-1 / count) {
      /* size * count would overflow size_t; treat exactly like any other exhausted allocation. */
      vkr_cs_decoder_set_fatal(dec);
      return NULL;
   }
   return vkr_cs_decoder_alloc_temp(dec, size * count);
}

/*
 * Blob storage: where a decoded "blob" array (raw bytes with no further structure — shader
 * specialization constants, push-constant values, pipeline-cache data) gets written.
 *
 * IMPLEMENTED FOR REAL — Task 2. Task 1 left this a NULL stub because nothing in Task 1 reached it;
 * Task 2 drives real `vn_decode_<command>_args_temp` functions, several of which (push constants,
 * pipeline specialization data, `vkGetPipelineCacheData`) carry a blob array, so a stub is no longer
 * safe. See the review finding this closes, recorded in the design spec and the Task 1 report.
 *
 * THE HAZARD THIS CLOSES: the generated call site (e.g.
 * `vn_protocol_renderer_pipeline_cache.h:vn_decode_VkPipelineCacheCreateInfo_self_temp`) reads:
 *
 *     val->pInitialData = vkr_cs_decoder_get_blob_storage(dec, array_size);
 *     if (!val->pInitialData) return;
 *     vn_decode_blob_array(dec, (void *)val->pInitialData, array_size);
 *
 * A NULL return takes the early `return` — which skips `vn_decode_blob_array`, the *only* call that
 * would advance the cursor past the blob and (via `vkr_cs_decoder_read`'s own bounds check) notice a
 * truncation. Mesa's generated code never calls `vkr_cs_decoder_set_fatal` on this path itself: it
 * simply trusts that `get_blob_storage` only refuses when there is a genuine reason, and reports
 * nothing further. In practice this never bites a real virglrenderer, because a well-formed client's
 * encoder always reserves enough room for what it writes; it can only be reached here by a stream
 * that is truncated, corrupt, or simply a byte range this crate was asked to look at without proof it
 * ends on a command boundary — precisely the input this diagnostic-only crate must expect, since
 * walking untrusted/partial byte ranges is its whole job. So: on the failure branch, we set fatal
 * OURSELVES, here, before returning NULL — closing the gap Mesa's own generated call site leaves
 * open, at the one place in the call graph that can still see it. `command_len` then reports
 * `RAYLAND_VENUS_FAULT_TRUNCATED` instead of silently under-consuming the stream, which is the
 * difference between an honest fault and the "confidently wrong length" this crate exists to avoid.
 *
 * On the success branch we match virglrenderer's own implementation exactly: hand back a pointer
 * straight into the decoder's own buffer (`dec->cur`) rather than a fresh copy. That is why
 * `vkr_cs_decoder_read`/`_peek`, above, carry the `dec->cur != val` aliasing guard — the generated
 * code immediately "decodes" this blob by reading from `dec->cur` into the very pointer we just
 * returned, i.e. `dst == src`, and that guard is what makes the resulting memcpy well-defined instead
 * of UB.
 */
static inline void *
vkr_cs_decoder_get_blob_storage(struct vkr_cs_decoder *dec, size_t size)
{
   if (size > (size_t)(dec->end - dec->cur)) {
      /* See the long comment above: this is the one place that can see the coming truncation before
       * the generated caller's early return would otherwise swallow it silently. */
      vkr_cs_decoder_set_fatal(dec);
      return NULL;
   }
   /* Enough room for the whole blob: point the caller at the bytes already sitting in our buffer,
    * exactly as virglrenderer's own `vkr_cs_decoder_get_blob_storage` does. Nothing is copied here;
    * the generated code's own subsequent read (see above) is what actually walks these bytes and
    * advances `dec->cur` past them. */
   return (void *)dec->cur;
}

/*
 * The encoder's counterpart to `vkr_cs_decoder_get_blob_storage`, above.
 *
 * CORRECTION, recorded here rather than silently fixed: the task brief that introduced this function
 * assumed "the encoder side is never driven by this crate" and allowed a documented no-op. That
 * assumption is FALSE for six commands at this vendored Mesa version — `vkGetQueryPoolResults`,
 * `vkGetPipelineCacheData`, `vkCopyImageToMemoryMESA`, `vkWriteAccelerationStructuresPropertiesKHR`,
 * `vkGetRayTracingShaderGroupHandlesKHR` and `vkGetRayTracingCaptureReplayShaderGroupHandlesKHR`.
 * Each of these has a `vn_decode_<command>_args_temp` — a *decode* function, in our call graph by
 * construction — whose generated signature is `(struct vn_cs_decoder *dec, struct vn_cs_encoder *enc,
 * struct vn_command_<command> *args)`: Mesa pre-allocates the *reply* arena's blob space during the
 * decode pass, so decoding these commands genuinely reaches this function. An `abort()` here (the
 * first-drafted version of this function) would crash on any stream containing one of them.
 *
 * WHY A NON-NULL SENTINEL IS SAFE ANYWAY: every one of the six call sites (verified by reading all
 * six, not sampling) has the identical shape:
 *
 *     args->pData = vn_cs_encoder_get_blob_storage(enc, offset, array_size);
 *     if (!args->pData) return;
 *
 * The pointer is only ever STORED into `args->pData` here; it is never dereferenced or written
 * through by the decode function itself. It becomes live only inside the matching `vn_encode_
 * <command>_reply` function — generated in the same header, and never called by this crate, which
 * calls `vn_decode_*_args_temp` and nothing else (see the design spec's architecture section). So any
 * non-null address discharges the contract these six call sites actually rely on, with no need to
 * back `size` bytes of real memory: sizing a real allocation would reintroduce exactly the "our own
 * scratch pool happened to be too small" exhaustion hazard `vkr_cs_decoder_get_blob_storage` above
 * had to close for the DECODE side — except here it would be needless, since nothing is ever written.
 * `vkGetPipelineCacheData` alone can carry a pipeline cache "dozens of MBs" large (per virglrenderer's
 * own comment on its temp-pool size limit), so refusing large requests would make this function
 * exactly the silent-under-report hazard it exists to avoid, for a size nothing will ever use.
 *
 * `sentinel` is `static` inside this `static inline` function: each translation unit gets its own
 * private copy with a stable address, which is all a never-dereferenced sentinel needs. `offset` and
 * `size` are intentionally unused for the reason above.
 */
static inline void *
vkr_cs_encoder_get_blob_storage(struct vkr_cs_encoder *enc, size_t offset, size_t size)
{
   (void)enc;
   (void)offset;
   (void)size;
   static uint8_t sentinel[1]; /* never read or written; only its non-null address matters */
   return (void *)sentinel;
}

#endif /* RAYLAND_VKR_CS_H */
