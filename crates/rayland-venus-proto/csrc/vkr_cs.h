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
 * Task 1's report for the discovery, and Task 2 before relying on the blob-storage stubs below.
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
 * STUB — DELIBERATELY NOT IMPLEMENTED. Task 1's only C entry point is a constant self-test; nothing in
 * Task 1 calls any `vn_decode_*_temp` function, so this body is never reached, and it exists purely so
 * the generated headers — which declare it `static inline` and are therefore type-checked
 * unconditionally by the compiler regardless of whether any caller in this crate reaches them — compile.
 *
 * Returning NULL here is safe for Task 1 but is NOT a free pass for Task 2: the generated caller
 * pattern is `val->pData = vkr_cs_decoder_get_blob_storage(...); if (!val->pData) return;` — a NULL
 * return skips the blob read *without* calling `vkr_cs_decoder_set_fatal`, unlike this file's other
 * exhaustion paths. Task 2 must give this a real body (most likely returning a bump-temp allocation, or
 * a pointer straight into the decoder's own buffer) before decoding any command whose payload includes
 * a blob array — until then, `command_len` would silently under-consume the stream for such commands
 * rather than reporting a fault, which is exactly the "confidently wrong" failure mode this crate exists
 * to avoid.
 */
static inline void *
vkr_cs_decoder_get_blob_storage(struct vkr_cs_decoder *dec, size_t size)
{
   (void)dec;
   (void)size;
   return NULL;
}

/*
 * The encoder's counterpart to `vkr_cs_decoder_get_blob_storage`, above.
 *
 * STUB — DELIBERATELY NOT IMPLEMENTED, for the same reason `vkr_cs_encoder_write` above is a no-op:
 * this crate never encodes. It exists only so the generated `vn_encode_*` helpers that reference it
 * type-check; Task 1 never calls any of them.
 */
static inline void *
vkr_cs_encoder_get_blob_storage(struct vkr_cs_encoder *enc, size_t offset, size_t size)
{
   (void)enc;
   (void)offset;
   (void)size;
   return NULL;
}

#endif /* RAYLAND_VKR_CS_H */
