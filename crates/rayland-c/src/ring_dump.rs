//! DIAGNOSTIC (`RAYLAND_RING_DUMP`), throwaway: name the commands in a ring delta, and in
//! particular which of them asked for a reply (`command_flags` bit 0). Venus aborts *silently* when
//! a reply decodes past its end — `vn_cs_decoder_set_fatal` is the only abort in the ICD that logs
//! nothing, and it reports no opcode, no sizes, nothing — so the only way to learn which command was
//! in flight is to decode the stream on this side, where the bytes still are. Inert unless
//! `RAYLAND_RING_DUMP` is set in the environment.
//!
//! # Why this diagnostic has its own module, away from `main.rs`'s relay path
//! This module holds `rayland-c`'s **only** call into `rayland_vtest::venus_ring::decode` — the
//! borrowed Venus decoder that (c)1 spec §7 requires stay diagnostic and structural only, never
//! load-bearing for what gets relayed. Before this module existed, that call sat inline inside
//! `main.rs`'s `ring_watcher_thread`, in the same function that also decides what to relay
//! (`messages_for_delta`, `scan_for_out_of_line_stream`, `link.send`) — reachable, but not yet used,
//! for a relay decision. A guard that only grepped for the decoder crate's name could not tell those
//! two facts apart, and could not have caught someone quietly making that existing call load-bearing
//! (for example, using its `stop` result to skip relaying a truncated command).
//!
//! Moving the call here makes the property checkable instead of merely true today: everything this
//! module does is `eprintln!`, nothing it computes is returned to a caller, and it cannot reach
//! `messages_for_delta`, `scan_for_out_of_line_stream`, or the network link — the things that
//! actually decide what crosses the wire. `rayland-c/tests/decoder_is_not_load_bearing.rs` asserts
//! that `main.rs` (along with `blob_sync.rs`, `ring.rs`, `relay_engine.rs`, `link.rs`) contains
//! neither the decoder crate's name nor a call to `decode_commands` — which it can now assert
//! truthfully, because the one legitimate call lives here instead.

use rayland_c::ring::RingDelta;
// The command types this diagnostic reports on, and the prologue width their arguments follow.
// Imported rather than re-spelled as literals: `venus_ring::decode` is where the evidence for each
// number lives, and a second copy here could drift from it silently.
use rayland_vtest::venus_ring::decode::{
    VK_COMMAND_TYPE_VK_DESTROY_DEVICE, VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2,
    VK_COMMAND_TYPE_VK_QUEUE_SUBMIT,
};

/// Width of a Venus command's `[type: u32][flags: u32]` prologue, so the first argument begins here.
///
/// Defined locally rather than imported: `venus_ring::decode`'s copy is private, and widening that
/// crate's public API for a diagnostic is the wrong trade. The value is fixed by the protocol — every
/// command starts with exactly those two `u32`s, which is what `vn_dispatch_command` reads before it
/// dispatches — so a local copy cannot drift in any way that matters.
const COMMAND_HEADER_BYTES: usize = 8;

/// Prints one diagnostic line per ring delta if `RAYLAND_RING_DUMP` is set in the environment;
/// otherwise returns immediately having done nothing — no decode, no allocation, no cost beyond the
/// one `env::var_os` check. Called from `ring_watcher_thread` for every delta the ring watcher
/// drains, so that early-out matters: the watcher's loop runs every few hundred microseconds during
/// an actively rendering application.
///
/// # What it prints
/// One line naming every command [`decode_commands`](rayland_vtest::venus_ring::decode::decode_commands)
/// could identify in this delta — its Venus command type, its byte offset, and whether it asked for
/// a reply — followed by a FNV-1a hash of the whole delta (see [`fnv1a`]) so the same bytes can be
/// recognised again on S's side of the wire when diagnosing a decode failure there. After that, a
/// raw scan for candidate `vkCreateRingMESA` / `vkDestroyRingMESA` / `vkNotifyRingMESA` dwords — see
/// the loop below for why that half is a scan rather than a second decode.
///
/// # Why this can never become load-bearing
/// It has no return value: nothing downstream of this call can observe what it decided, because it
/// never decides anything — it only writes to stderr. It never touches `messages_for_delta`,
/// `scan_for_out_of_line_stream`, or the `tx` link the ring watcher sends over. That absence, not a
/// promise in prose, is what makes it safe to call from inside the relay's hot loop.
pub fn dump_if_enabled(pending: &RingDelta) {
    // Cheapest possible early-out: this diagnostic must cost nothing when nobody asked for it.
    if std::env::var_os("RAYLAND_RING_DUMP").is_none() {
        return;
    }
    // The one call in this crate to the borrowed Venus decoder. Its result is used only to build the
    // strings printed below — nothing here is returned to the caller.
    let (commands, stop) = rayland_vtest::venus_ring::decode::decode_commands(&pending.bytes);
    // One line per delta: the reply-bearing commands are the candidates for the abort, so mark them
    // rather than making a reader cross-reference the flags.
    let named: Vec<String> = commands
        .iter()
        .map(|c| {
            let reply = if c.command_flags & 1 != 0 { " REPLY" } else { "" };
            format!("@{} type={}{}", c.offset, c.command_type, reply)
        })
        .collect();
    eprintln!(
        "[ring-cmds] tail={} len={} fnv={:016x} {} cmd(s) stop={:?}: {}",
        pending.tail,
        pending.bytes.len(),
        fnv1a(&pending.bytes),
        commands.len(),
        stop,
        named.join(" | ")
    );
    // **The queue-identity question.** S refuses the application's `vkQueueSubmit` with a
    // "CS error", and the generated dispatcher sets that same fatal flag on
    // `if (!args.queue)` — a *handle lookup failure*, not a read past the end
    // (`vn_protocol_renderer_queue.h:1141-1145`). So the question is whether the id the submit
    // names is the id the application created. Both are in these bytes, and the walk above now
    // reaches them, so print them side by side and let the ids answer it.
    for cmd in &commands {
        // The queue a submit names: `vn_decode_vkQueueSubmit_args_temp` opens with
        // `vn_decode_VkQueue_lookup`, so the object id is the `u64` immediately after the
        // `[type][flags]` prologue.
        if cmd.command_type == VK_COMMAND_TYPE_VK_QUEUE_SUBMIT {
            if let Some(id) = read_u64_at(&pending.bytes, cmd.offset + COMMAND_HEADER_BYTES) {
                eprintln!("[ring-queue] submit @{} names queue id {id:#x}", cmd.offset);
            }
            continue;
        }
        // The queue the application created: `vn_decode_vkGetDeviceQueue2_args_temp` decodes
        // `device`, then `pQueueInfo`, then `pQueue` via `vn_decode_VkQueue_temp` — the *creation*
        // form, carrying the client-chosen id, and it is decoded last. So the id is the command's
        // final eight bytes (`vn_protocol_renderer_device.h:25397-25411`).
        if cmd.command_type == VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2 {
            let end = cmd.offset + cmd.encoded_size;
            if let Some(id) = read_u64_at(&pending.bytes, end.wrapping_sub(8)) {
                eprintln!("[ring-queue] GetDeviceQueue2 @{} creates queue id {id:#x}", cmd.offset);
            }
            continue;
        }
        // A device destroy takes its queues with it, so it is the other way the submit's id could
        // become unresolvable. Its handle is the first argument, same position as the submit's.
        if cmd.command_type == VK_COMMAND_TYPE_VK_DESTROY_DEVICE {
            if let Some(id) = read_u64_at(&pending.bytes, cmd.offset + COMMAND_HEADER_BYTES) {
                eprintln!("[ring-queue] DestroyDevice @{} destroys device id {id:#x}", cmd.offset);
            }
        }
    }
    // **Is S's `find_destroy_device` telling the truth?** S logs "application destroyed its device"
    // and acts on it (retiring the readback gate), but a full decode of this same stream finds no
    // `vkDestroyDevice` at all. The two disagree, and they differ in kind: `find_destroy_device` is a
    // sliding 16-byte pattern match (`[type=12][flags=0][device_handle]`) at arbitrary positions,
    // while the walk above decodes commands at their real boundaries. Running both over the *same*
    // bytes settles it — if the scan's hit is not one of the decoded offsets above, it matched inside
    // a payload and S has been acting on a phantom.
    //
    // The device handle is remembered from whichever `vkGetDeviceQueue2` crossed earlier, because the
    // scan needs it as its discriminator and it is the same handle S uses.
    for cmd in &commands {
        if cmd.command_type == VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2 {
            if let Some(device) = read_u64_at(&pending.bytes, cmd.offset + COMMAND_HEADER_BYTES) {
                LAST_DEVICE_HANDLE.store(device, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[ring-queue] GetDeviceQueue2 @{} device handle {device:#x}", cmd.offset);
            }
        }
    }
    let device = LAST_DEVICE_HANDLE.load(std::sync::atomic::Ordering::Relaxed);
    if device != 0 {
        if let Some(hit) = rayland_vtest::venus_ring::decode::find_destroy_device(&pending.bytes, device) {
            // The verdict, stated in the log so no one has to cross-reference by hand: a hit that is
            // not a decoded command offset is a false positive by construction.
            let on_boundary = commands.iter().any(|c| c.offset == hit);
            let decoded_type = commands
                .iter()
                .find(|c| c.offset == hit)
                .map_or("none".to_string(), |c| c.command_type.to_string());
            eprintln!(
                "[ring-destroy] find_destroy_device FIRED at offset {hit} for device {device:#x} \
                 — on a decoded command boundary: {on_boundary} (decoder says type={decoded_type}); \
                 tail={}",
                pending.tail
            );
        }
    }

    // **And is `find_get_device_queue2` any better?** It is built the same way as
    // `find_destroy_device` — a sliding pattern match rather than a decode — and that one was just
    // proved to fire inside a payload. This crate's decoder can now say, for the same bytes, whether
    // the scan's hit corresponds to a real `vkGetDeviceQueue2`: the struct reports `end_offset` (the
    // first byte past the command), so a truthful hit must line up with a decoded type-155 command's
    // offset plus its size. A hit in a delta containing no type-155 at all is a false positive with
    // nothing further to argue about.
    if let Some(found) = rayland_vtest::venus_ring::decode::find_get_device_queue2(&pending.bytes) {
        let real = commands
            .iter()
            .find(|c| c.command_type == VK_COMMAND_TYPE_VK_GET_DEVICE_QUEUE2);
        let agrees = real.is_some_and(|c| c.offset + c.encoded_size == found.end_offset);
        eprintln!(
            "[ring-queuescan] find_get_device_queue2 FIRED: end_offset={} ring_idx={} device={:#x} \
             — decoder found a real type-155 in this delta: {} (offsets agree: {}); tail={}",
            found.end_offset,
            found.ring_idx,
            found.device_handle,
            real.is_some(),
            agrees,
            pending.tail
        );
    }

    // **The multi-ring question.** Thread sampling on S found three `vkr-ring-1` threads where (c)1
    // spec §6 assumes one, and S never saw a second *inline* `vkCreateRingMESA` — so any extra ring
    // must be created inside the ring stream, which is exactly what this scans for. `decode_commands`
    // cannot walk far enough to find them (it halts at the first unknown-size command, which is the
    // application's own), so this is a direct scan of the delta's dwords for the
    // create/destroy/notify command types. A bare dword match can collide with payload data, so the
    // offset is printed and the count is what matters, not any single hit: a real ring creation
    // should coincide with a ring thread appearing.
    for off in (0..pending.bytes.len().saturating_sub(3)).step_by(4) {
        let word = u32::from_le_bytes([
            pending.bytes[off],
            pending.bytes[off + 1],
            pending.bytes[off + 2],
            pending.bytes[off + 3],
        ]);
        let name = match word {
            188 => "vkCreateRingMESA",
            189 => "vkDestroyRingMESA",
            190 => "vkNotifyRingMESA",
            _ => continue,
        };
        eprintln!("[ring-life] tail={} off={off} candidate={name} ({word})", pending.tail);
    }
}

/// FNV-1a over a byte slice — a **diagnostic** fingerprint of one ring delta.
///
/// # Why this exists
/// `rayland-s` refuses the application's `vkQueueSubmit` with a virglrenderer "CS error", which is a
/// *decode* failure: the bytes S parsed were not the bytes it needed. There are two ways that
/// happens, and they call for completely different fixes — either the ring relay corrupted or
/// truncated the delta on its way across, or the relay is faithful and the submit refers to bytes
/// that never travel in the ring at all (Venus's staging pool, which `crate::blob_sync` declines to
/// publish by design). Hashing the same delta on both sides and comparing per `tail` separates those
/// two with one run. See `docs/DIARY.md`, 2026-07-26.
///
/// # Why FNV-1a rather than anything stronger
/// This compares two byte strings that are either identical or badly different; it is not defending
/// against an adversary choosing collisions. FNV-1a is a dozen lines, has no dependency, and runs at
/// memory speed — which matters because this runs once per delta while `RAYLAND_RING_DUMP` is set,
/// and an instrument that slows the thing it measures is how the last wall was misread for two days.
///
/// # Inputs / outputs
/// - `bytes`: the delta exactly as relayed.
/// - Returns the 64-bit hash. Pure; no allocation, no failure mode.
fn fnv1a(bytes: &[u8]) -> u64 {
    // The standard 64-bit FNV offset basis and prime.
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        // XOR *then* multiply — that ordering is what makes this FNV-1a rather than FNV-1, and the
        // two give different digests, so a reader comparing against another implementation needs it.
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Read a little-endian `u64` at `offset`, or `None` if fewer than eight bytes remain.
///
/// Little-endian regardless of the host, for the same reason the ring decoder pins it: these bytes
/// are a memory image of the machine that wrote them, and decoding them natively would make this
/// diagnostic silently wrong on a big-endian client — which this project explicitly targets.
///
/// # Inputs / outputs
/// - `bytes`: the delta as relayed; `offset`: where the field begins.
/// - Returns the value, or `None` when the slice is too short. **Never panics** — a truncated read
///   is a missing log line, not a crash in a diagnostic.
fn read_u64_at(bytes: &[u8], offset: usize) -> Option<u64> {
    let field = bytes.get(offset..offset.checked_add(8)?)?;
    // `try_into` cannot fail: the slice was just bounds-checked to exactly eight bytes.
    Some(u64::from_le_bytes(field.try_into().ok()?))
}

/// The most recent `VkDevice` handle seen crossing in a `vkGetDeviceQueue2`.
///
/// Held in a static because `find_destroy_device` needs the handle as its discriminator, and the
/// destroy may arrive in a *later* delta than the queue acquisition — so the value has to outlive the
/// call that learned it. Relaxed ordering is right: this is a diagnostic, a stale read costs at worst
/// one unchecked delta, and the watcher is the only writer.
static LAST_DEVICE_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
