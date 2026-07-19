//! (c)1 Task 9 stage tracing: a shared, env-gated timeline probe for the stale-frame race.
//!
//! # Why this module exists, and why it lives here
//! `docs/design/2026-07-17-return-path-completion.md` §7 asks, before any fix is attempted, that the
//! return path's stages be timestamped **separately** so the ordering graph can be read off directly
//! — specifically to find where it permits `T9 < T7` (the application reads before its pixels are
//! installed on C) or `T2 < T4` (the code treats a ring/fence retirement as proof the GPU's readback
//! is complete, and the evidence says that is false). Those stages straddle two processes —
//! `rayland-s` (which owns the GPU) and `rayland-c` (which owns the application) — so the two logs
//! must share **one clock** and **one line format** or they cannot be joined. This module is that
//! shared foundation, and it lives in `rayland-relay` for the same reason the message set does: it is
//! the one crate both daemons already depend on, and putting it anywhere else would either duplicate
//! it (and let the two copies drift — the exact failure the icosa `-core`/`-vk` crates exist to
//! prevent) or force a dependency edge that does not otherwise exist.
//!
//! # It stays within `rayland-relay`'s purity contract
//! The crate is "pure data: no GPU code, no sockets, no async runtime." This module adds none of
//! those. It reads a monotonic clock (a `clock_gettime` syscall, not a GPU call — the same reasoning
//! `rayland-vtest` records for depending on `libc`), checks an environment variable once, and writes
//! diagnostic lines to **stderr**. Stderr is not network I/O and not a socket, so no statement in the
//! crate's manifest or module docs is made false by it. It is nonetheless *diagnostic* code, gated
//! off by default, and is expected to be removed or pared back once the ordering graph has served its
//! purpose and the real fix (a completion protocol) is in.
//!
//! # The one clock, and why raw monotonic nanoseconds rather than an `Instant`
//! Both daemons stamp events with [`monotonic_ns`], which reads `CLOCK_MONOTONIC`. On Linux that
//! clock is a **single system-wide timebase**: two processes on one machine read the same zero, so
//! their timestamps are directly comparable without any handshake. `std::time::Instant` deliberately
//! hides its absolute value, so it cannot be compared across processes; the raw counter can. The
//! (c)1 measurement runs on loopback — both daemons on one host — so this is exactly the regime where
//! a shared monotonic counter joins the two logs perfectly. (Across a real network the two clocks are
//! independent and this join no longer holds; that is a separate problem the completion protocol,
//! not this instrument, must solve.)

use std::sync::OnceLock;

/// The environment variable that turns tracing on. Absent → every [`emit`] is a cheap early return.
///
/// An environment variable rather than a flag for the same reason `RAYLAND_C1_METRICS` is one: these
/// daemons are launched by a test harness and, in the field, by an ssh command line assembled on
/// another machine, where an inherited environment is the one control that reaches both ends without
/// re-plumbing every call site.
const ENV_TRACE: &str = "RAYLAND_C1_TRACE";

/// Whether stage tracing is enabled, decided once from [`ENV_TRACE`] and cached.
///
/// Cached in a `OnceLock` because [`emit`] is called on the return path's hot loops (every 200 µs
/// poll on S, and per message on C), and re-reading the environment on each call would be both slower
/// and — since a process's environment can be mutated — less predictable than latching the decision
/// at first use.
///
/// # Returns
/// `true` if [`ENV_TRACE`] is present in the environment (any value, including empty), `false`
/// otherwise.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(ENV_TRACE).is_some())
}

/// Read the system-wide monotonic clock, in nanoseconds since an unspecified but fixed epoch.
///
/// # Why this and not `Instant`
/// See the module docs: this returns the *absolute* `CLOCK_MONOTONIC` counter, which is comparable
/// across processes on the same host, whereas `Instant` hides it. The epoch is arbitrary (typically
/// boot), so only **differences** are meaningful — but a difference between a stamp taken in S and
/// one taken in C is exactly what the ordering graph needs.
///
/// # Returns
/// The monotonic time in nanoseconds. On the astronomically unlikely event that `clock_gettime`
/// fails, returns 0 rather than panicking: this is diagnostic code and must never be the thing that
/// takes down a session.
pub fn monotonic_ns() -> u64 {
    // SAFETY: `ts` is a live, correctly-typed `timespec` we hand the kernel to fill. `clock_gettime`
    // writes it and touches nothing else; we read it only on success.
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        // A failing monotonic clock is not a reason to abort a render session; return a sentinel and
        // let the offline join notice the zero.
        return 0;
    }
    // Fold seconds and nanoseconds into one counter. u64 nanoseconds covers ~584 years of uptime, so
    // this cannot overflow on any real machine.
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

/// A cheap, strided fingerprint of a blob region, for detecting *that* its contents changed.
///
/// # Why strided rather than a full hash
/// Probe A (see `rayland-s`'s progress loop) re-fingerprints the application's readback blob on
/// **every** 200 µs poll while the application is blocked, to catch S's GPU still writing after the
/// return path declared the work retired. A full hash of a ~1 MiB blob costs milliseconds — more than
/// the poll interval itself — so hashing every poll would dominate S's CPU and, worse, perturb the
/// very race being measured. Sampling one byte per cache line makes the cost microseconds and flat,
/// at the price of being a change *detector* rather than a collision-resistant digest. That trade is
/// exactly right here: both failure modes this instrument hunts change memory in bulk — a stale frame
/// replaces the whole buffer, and a torn frame rewrites large contiguous regions — so a lattice of
/// samples across the buffer will see the change with overwhelming probability. It is emphatically
/// **not** suitable for proving two buffers are *equal*; nothing here asks it to.
///
/// # Inputs / outputs
/// - `bytes`: the region to fingerprint.
/// - Returns a 64-bit value that changes with overwhelming probability when a bulk write lands, and
///   is stable across calls on unchanged memory. The blob's length is folded in, so a truncation or
///   extension changes the fingerprint even if every sampled byte coincides.
pub fn fingerprint(bytes: &[u8]) -> u64 {
    // FNV-1a's constants: a well-understood, dependency-free mixing function. We are detecting change,
    // not defending against a crafted collision, so its non-cryptographic strength is ample.
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    // One sample per 64-byte cache line: the same granularity Mesa's `alignas(64)` on the ring's
    // control words assumes, and small enough that a 1 MiB blob is ~16k cheap iterations.
    const STRIDE: usize = 64;

    let mut hash = FNV_OFFSET;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += STRIDE;
    }
    // Fold the length so that "the same sampled bytes but a different size" is still a different
    // fingerprint — cheap insurance against a resize that happens to align with the stride.
    hash ^= bytes.len() as u64;
    hash.wrapping_mul(FNV_PRIME)
}

/// Emit one trace line, if tracing is enabled.
///
/// # The line format, and why it is `key=value`
/// Every line is `RLTRACE t_ns=<monotonic> stage=<stage> <fields>`, where `<fields>` is whatever
/// `key=value` string the call site built. A flat `key=value` shape is trivially greppable and
/// trivially parsed by an offline joiner (`docs/c1-the-network.md`'s analysis scripts), and it lets
/// each call site name exactly the fields its stage has — the readback-blob stages carry `res=`/`off=`
/// and a fingerprint, the ring stages carry a `tail=`, and there is no need for a rigid schema across
/// stages that have genuinely different things to say.
///
/// The timestamp is read *inside* this function, as late as possible before the write, so the stamp
/// is of the event and not of whatever formatting the caller did to build `fields`.
///
/// # Inputs / outputs
/// - `stage`: the pipeline stage this event belongs to — `T0`, `T2`, `T5`, `T6`, `T7`, `T8`, or a
///   probe label such as `A_RESAMPLE`. Matches the naming in the design note's §7.
/// - `fields`: a pre-built `key=value` string (space-separated) with this stage's specifics. The
///   caller owns its content, including the `side=S`/`side=C` tag that says which daemon spoke.
/// - Returns nothing. When tracing is disabled this is a single boolean load and return.
pub fn emit(stage: &str, fields: &str) {
    if !enabled() {
        return;
    }
    // One `eprintln!` is one `write(2)` under the hood, so a whole line lands atomically against
    // other threads' lines even without a lock — good enough for a diagnostic, and it keeps this
    // function free of any shared state a session could contend on.
    eprintln!("RLTRACE t_ns={} stage={stage} {fields}", monotonic_ns());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fingerprint must be stable on unchanged input and move on a bulk change — the two
    /// properties Probe A relies on. A single flipped byte that the stride happens to skip is
    /// *allowed* to go unnoticed (this is a detector, not a digest), so the change tested here is a
    /// bulk one, which is what the real failure modes produce.
    #[test]
    fn fingerprint_is_stable_and_moves_on_bulk_change() {
        let a = vec![0u8; 4096];
        assert_eq!(fingerprint(&a), fingerprint(&a), "stable on identical input");

        let mut b = a.clone();
        for byte in b.iter_mut() {
            *byte = 0xff;
        }
        assert_ne!(
            fingerprint(&a),
            fingerprint(&b),
            "a whole-buffer rewrite — the stale-frame case — must change the fingerprint"
        );
    }

    /// Length is folded in, so two buffers that agree on every sampled byte but differ in size are
    /// still distinguishable.
    #[test]
    fn fingerprint_folds_length() {
        let short = vec![0u8; 64];
        let long = vec![0u8; 128];
        assert_ne!(fingerprint(&short), fingerprint(&long));
    }

    /// The monotonic clock must not go backwards between two reads. This is the one guarantee the
    /// cross-process join rests on.
    #[test]
    fn monotonic_ns_does_not_go_backwards() {
        let first = monotonic_ns();
        let second = monotonic_ns();
        assert!(second >= first, "CLOCK_MONOTONIC must be non-decreasing");
    }
}
