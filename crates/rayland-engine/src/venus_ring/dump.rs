//! A **live diagnostic**: watches the shared pages behind a blob resource and reports what a real
//! Venus client writes into them. This is the instrument that made the parent module's finding.
//!
//! # Why this exists
//! The vtest wire protocol carries almost no command traffic: a live init-only client produces only
//! a handful of `VCMD_SUBMIT_CMD2` messages, far too few to describe a full Vulkan instance+device
//! bring-up. The traffic must therefore be going somewhere else. The only other channel between the
//! client and us is the **blob resource**: shared memory whose descriptor we hand the client, which
//! it then `mmap`s and writes into directly — bytes that never touch the socket.
//!
//! Since *we* export that descriptor (`virgl_renderer_resource_export_blob`, `fd_type = SHM`), we
//! can `mmap(MAP_SHARED)` the very same physical pages and watch them evolve. That is what this
//! module does, and it is the only way to see the bytes at all.
//!
//! # Relationship to the rest of `venus_ring`
//! This module is the *observer*; it prints hex and per-dword behaviour and draws no conclusions.
//! [`super::decode`] is the *interpreter*, and [`super::RING_HEAD_OFFSET`] and friends record the
//! layout that observation revealed. The split is deliberate: the decoder must be testable without
//! a GPU (it is — see the parent module's `captured` fixture), while this half is inherently a
//! live-hardware instrument and can only ever be run by hand.
//!
//! # How it works
//! [`watch_blob`] maps a blob's pages read-only and registers them; the first registration starts a
//! background sampler thread that snapshots every mapping every [`SAMPLE_INTERVAL`] and prints
//! whatever changed since the previous sample. Sampling (rather than dumping once at teardown) is
//! essential: a ring is a *circular* buffer whose contents are consumed and overwritten, so a single
//! end-of-run dump would show a drained or wrapped ring and tell us nothing about the traffic.
//!
//! [`doorbell`] is called from the `VCMD_SUBMIT_CMD2` dispatch path so the socket-side messages can
//! be correlated against the ring-side byte movement on one timeline.
//!
//! # Everything here is gated behind `RAYLAND_RING_DUMP`
//! Unset (the default), every entry point returns immediately after one `var_os` lookup and nothing
//! is mapped, no thread is spawned, and nothing is printed. This mirrors `vtest::dump_submit_cmd2`'s
//! style deliberately. Nothing in this module is on any engine code path; it only ever observes.
//!
//! # Honest caveats
//! - The sampler races the writer by construction. A dword may be observed torn or a burst of
//!   writes may be missed entirely between samples. Counts printed here are therefore **lower
//!   bounds** on activity, never exact.
//! - Reads are `read_volatile` per dword, so the compiler cannot cache or elide them, but this is
//!   still a data race in the Rust abstract machine's terms (another *process* is writing). That is
//!   acceptable for a diagnostic that only ever observes; it would not be acceptable in shipped
//!   code.

// The descriptor to map, borrowed for the duration of the `mmap` call only (the kernel keeps its
// own reference to the underlying object afterwards).
use std::os::fd::{AsRawFd, BorrowedFd};
// The registry of watched mappings, shared between the dispatch thread and the sampler thread.
use std::sync::{Mutex, OnceLock};
// Doorbell counters, updated from the dispatch thread and read by the reporter.
use std::sync::atomic::{AtomicU64, Ordering};
// Sample pacing and the single shared timeline every printed line is stamped against.
use std::time::{Duration, Instant};

/// The environment variable that enables everything in this module. Set to any non-empty value.
const RING_DUMP_ENV_VAR: &str = "RAYLAND_RING_DUMP";

/// How often the sampler thread snapshots every watched mapping.
///
/// 1 ms is a compromise with no clean answer: the client writes at memory speed, so *no* sampling
/// rate can catch every intermediate state. Fast enough to see a bring-up sequence (which spans
/// tens of milliseconds) evolve in many steps; slow enough that the sampler does not saturate a core
/// and perturb the very timing it is measuring.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(1);

/// How many bytes at the start of a mapping are treated as the suspected **control area** and
/// reported dword-by-dword (rather than merely as a changed range).
///
/// 256 is chosen to comfortably cover the arithmetic that motivated the investigation: the client's
/// first blob is **131268** bytes = 131072 (128 KiB, a power of two — a plausible data buffer) +
/// **196** bytes left over. 196 bytes of non-power-of-two remainder is what a *header* looks like.
///
/// Whether that header sat at the start, at the end, or was not a header at all was the open
/// question this dump was built to answer without assuming — which is why the region is reported
/// verbatim and the mapping's tail is reported too, rather than the front alone. The answer is now
/// known (192 bytes of control at the front; see [`super::RING_BUFFER_OFFSET`]), but this module
/// deliberately keeps reporting both ends and keeps hardcoding nothing: its whole value is being an
/// instrument that can still surprise us on a client that does something different.
const CONTROL_PREFIX_BYTES: usize = 256;

/// One blob mapping under observation, plus the state needed to diff it against its own past.
struct Watched {
    /// The engine-assigned resource id, so a printed line names which blob it is about.
    resource_id: u32,
    /// Base address of our `MAP_SHARED` mapping of the client's pages. Read-only; never written.
    ptr: *const u32,
    /// Length of the mapping in dwords (the blob's size is always dword-aligned in practice; a
    /// trailing partial dword, if one ever existed, is deliberately not sampled).
    dwords: usize,
    /// The previous sample, for diffing. Empty until the first sample has been taken.
    prev: Vec<u32>,
    /// Per-dword statistics for the suspected control area only (`CONTROL_PREFIX_BYTES / 4` entries
    /// at the front, plus the final 16 dwords of the mapping — the two places a header could be).
    /// Tracks how a dword *behaves*, which is what distinguishes a head/tail index (monotonic
    /// counter) from payload (arbitrary).
    stats: Vec<DwordStats>,
    /// A full snapshot taken at the first sample in which **any** dword outside the control prefix
    /// was non-zero — i.e. the earliest evidence of real data, before a ring has had a chance to
    /// wrap or be drained. This is the snapshot the final hex dump prints.
    first_data_snapshot: Option<Vec<u32>>,
    /// Milliseconds since the shared timeline start at which `first_data_snapshot` was taken.
    first_data_at_ms: f64,
    /// Highest dword index ever observed non-zero — a proxy for how much of the buffer is actually
    /// used, which is meaningless as a single sample but meaningful as a high-water mark.
    high_water_dword: Option<usize>,
    /// Count of distinct dword indices that ever changed value between two samples. A lower bound
    /// on how much of the mapping is live (see the module's caveats).
    ever_changed: usize,
    /// Which dword indices have already been counted in `ever_changed`.
    changed_seen: Vec<bool>,
    /// How many samples observed at least one change.
    changed_samples: u64,
}

// SAFETY: `Watched` holds a raw pointer, which makes it `!Send` by default. The mapping it names
// belongs to the *process*, not to the thread that created it, and this module only ever *reads*
// through the pointer. Handing the registry to the sampler thread is therefore sound with respect
// to our own address space; the cross-process race with the client's writes is discussed in the
// module docs and is inherent to the observation, not introduced by moving the pointer.
unsafe impl Send for Watched {}

/// How one dword in the suspected control area behaves over the whole run — the evidence that
/// separates "this is a monotonically increasing index" from "this is arbitrary payload".
#[derive(Clone, Copy)]
struct DwordStats {
    /// The dword's byte offset in the mapping, so a report line is self-describing.
    byte_offset: usize,
    /// How many times this dword's value changed between consecutive samples.
    changes: u64,
    /// Lowest value ever observed.
    min: u32,
    /// Highest value ever observed.
    max: u32,
    /// The value at the previous sample, for the monotonicity test.
    last: u32,
    /// True while every observed change has been an increase. A head/tail index that wraps modulo
    /// the buffer size would break this and *say so*, which is a finding either way.
    monotonic_up: bool,
    /// Whether this dword has been sampled at least once (so `min`/`max` are meaningful).
    seen: bool,
}

/// The registry of watched mappings. `None` until the first [`watch_blob`] call with the env var set.
static WATCHED: OnceLock<Mutex<Vec<Watched>>> = OnceLock::new();

/// The single timeline every printed line is stamped against, started at the first entry point call
/// so ring samples and socket doorbells are directly comparable.
static START: OnceLock<Instant> = OnceLock::new();

/// How many `VCMD_SUBMIT_CMD2` messages have arrived on the socket.
static DOORBELL_MESSAGES: AtomicU64 = AtomicU64::new(0);
/// How many `SUBMIT_CMD2` batches have arrived (a message may carry several).
static DOORBELL_BATCHES: AtomicU64 = AtomicU64::new(0);
/// Total command-stream dwords carried **inline** on the socket by those batches — the quantity the
/// ring's byte movement is to be compared against.
static DOORBELL_CMD_DWORDS: AtomicU64 = AtomicU64::new(0);

/// Whether this diagnostic is enabled. One `var_os` lookup; everything else short-circuits on it.
fn enabled() -> bool {
    std::env::var_os(RING_DUMP_ENV_VAR).is_some()
}

/// Milliseconds since the shared timeline started, for stamping a printed line.
fn now_ms() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0
}

/// Maps a blob's shared pages read-only and starts watching them.
///
/// # Inputs / outputs
/// - `resource_id`: the engine-assigned id, used only to label output.
/// - `fd`: the descriptor about to be sent to the client — the *same* memory object it will `mmap`.
///   Borrowed: `mmap` takes its own reference, so the caller may still send and close it.
/// - `size`: the blob's size in bytes, as the client requested it.
///
/// Returns nothing and **never fails the caller**: a mapping failure is printed and ignored, because
/// a diagnostic must not be able to break the session it is diagnosing.
pub fn watch_blob(resource_id: u32, fd: BorrowedFd<'_>, size: u64) {
    if !enabled() {
        return;
    }
    // Start (or read) the shared timeline before the first stamped line is printed.
    let t = now_ms();

    let Ok(len) = usize::try_from(size) else {
        eprintln!(
            "[ring +{t:8.2}ms] res={resource_id}: size {size} does not fit a usize; not watched"
        );
        return;
    };
    // A sub-dword blob has nothing dword-structured to report; refuse rather than mis-sample.
    if len < 4 {
        eprintln!("[ring +{t:8.2}ms] res={resource_id}: size {size} is too small to sample");
        return;
    }

    // Map the client's pages into *this* process. `MAP_SHARED` is the entire point — a `MAP_PRIVATE`
    // mapping would copy-on-write and we would watch a frozen private copy, seeing nothing the
    // client ever writes. `PROT_READ` only: this is an observer, and the kernel enforcing that is
    // better than a comment promising it.
    // SAFETY: null `addr` lets the kernel choose; `fd` is a live SHM object of at least `len` bytes
    // (it was exported for exactly this blob's `size`).
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        // A failure here is itself a finding (it would mean the exported fd is not mappable by us),
        // so report it loudly rather than silently skip the blob.
        eprintln!(
            "[ring +{t:8.2}ms] res={resource_id}: mmap of {size} bytes FAILED: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

    let dwords = len / 4;
    // The dword indices whose behaviour is tracked in detail: the suspected header at the front, and
    // the last 16 dwords, because a control area could equally live at the *end* of the mapping and
    // assuming otherwise would be exactly the kind of forced conclusion this instrument must avoid.
    let prefix = (CONTROL_PREFIX_BYTES / 4).min(dwords);
    let tail_start = dwords.saturating_sub(16).max(prefix);
    let tracked: Vec<usize> = (0..prefix).chain(tail_start..dwords).collect();
    let stats = tracked
        .iter()
        .map(|&i| DwordStats {
            byte_offset: i * 4,
            changes: 0,
            min: u32::MAX,
            max: 0,
            last: 0,
            monotonic_up: true,
            seen: false,
        })
        .collect();

    eprintln!(
        "[ring +{t:8.2}ms] WATCHING res={resource_id} size={size} bytes ({dwords} dwords) mapped at {ptr:p}"
    );

    let registry = WATCHED.get_or_init(|| Mutex::new(Vec::new()));
    // A poisoned mutex would mean the sampler thread panicked; there is nothing useful to do about
    // it in a diagnostic, so recover the data and carry on rather than take the session down.
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    // First registration starts the sampler. Doing it here (rather than at module init) is what
    // keeps the whole diagnostic inert when the env var is unset.
    let first = guard.is_empty();
    guard.push(Watched {
        resource_id,
        ptr: ptr as *const u32,
        dwords,
        prev: Vec::new(),
        stats,
        first_data_snapshot: None,
        first_data_at_ms: 0.0,
        high_water_dword: None,
        ever_changed: 0,
        changed_seen: vec![false; dwords],
        changed_samples: 0,
    });
    drop(guard);

    if first {
        spawn_sampler();
    }
}

/// Records that a `VCMD_SUBMIT_CMD2` message arrived on the socket, and prints it on the same
/// timeline as the ring samples.
///
/// This is the **doorbell cross-check**: if the ring is where the traffic is, then these messages
/// should be few and small while the ring moves a lot of bytes. `cmd_dwords` is the total inline
/// command-stream size across the message's batches — the socket-side quantity to compare against.
pub fn doorbell(batches: u64, cmd_dwords: u64) {
    if !enabled() {
        return;
    }
    let n = DOORBELL_MESSAGES.fetch_add(1, Ordering::Relaxed) + 1;
    DOORBELL_BATCHES.fetch_add(batches, Ordering::Relaxed);
    DOORBELL_CMD_DWORDS.fetch_add(cmd_dwords, Ordering::Relaxed);
    eprintln!(
        "[ring +{:8.2}ms] DOORBELL #{n}: SUBMIT_CMD2 with {batches} batch(es), {cmd_dwords} inline cmd dwords ({} bytes)",
        now_ms(),
        cmd_dwords * 4
    );
}

/// Starts the background sampler thread. Called exactly once, from the first [`watch_blob`].
///
/// The thread runs until the process exits — deliberately: there is no clean "session over" signal
/// available here, and a detached diagnostic thread that outlives its usefulness costs nothing in a
/// diagnostic harness that exits moments later.
fn spawn_sampler() {
    std::thread::spawn(|| {
        loop {
            sample_once();
            std::thread::sleep(SAMPLE_INTERVAL);
        }
    });
}

/// Reads every watched mapping once and prints whatever changed since the previous read.
fn sample_once() {
    let Some(registry) = WATCHED.get() else {
        return;
    };
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());
    for w in guard.iter_mut() {
        sample_watched(w);
    }
}

/// Snapshots one mapping, diffs it against the previous snapshot, updates statistics, and prints a
/// line if anything moved.
fn sample_watched(w: &mut Watched) {
    let t = now_ms();
    // Volatile, dword at a time: the memory is written by another process, so the compiler must not
    // be allowed to assume it is unchanging or to coalesce reads.
    let mut snap = Vec::with_capacity(w.dwords);
    for i in 0..w.dwords {
        // SAFETY: `ptr` is a live `PROT_READ` mapping of `dwords * 4` bytes; `i < dwords`. The value
        // read may be torn with respect to the writer, which the module docs call out — a torn read
        // is a wrong *value*, never an invalid access.
        snap.push(unsafe { std::ptr::read_volatile(w.ptr.add(i)) });
    }

    // Update the per-dword statistics for the tracked control candidates.
    for s in w.stats.iter_mut() {
        let v = snap[s.byte_offset / 4];
        if !s.seen {
            // First observation establishes the baseline; it is not a "change".
            s.seen = true;
            s.min = v;
            s.max = v;
            s.last = v;
            continue;
        }
        if v != s.last {
            s.changes += 1;
            // A decrease refutes "this is a monotonic counter" — record that rather than hide it.
            if v < s.last {
                s.monotonic_up = false;
            }
            s.last = v;
        }
        s.min = s.min.min(v);
        s.max = s.max.max(v);
    }

    // The first sample only establishes a baseline; there is nothing to diff against yet.
    if w.prev.is_empty() {
        w.prev = snap;
        return;
    }

    // Collect the dword indices that moved since the previous sample. Zipped rather than indexed so
    // the two snapshots are walked in lockstep by construction; they are always the same length,
    // and a length mismatch could only come from a bug that this would otherwise index past.
    let mut changed: Vec<usize> = Vec::new();
    for (i, (now, before)) in snap.iter().zip(w.prev.iter()).enumerate() {
        if now != before {
            changed.push(i);
            // First time this dword ever moved: count it toward the "how much of the mapping is
            // live" lower bound. Subsequent moves of the same dword must not be double-counted.
            if !w.changed_seen[i] {
                w.changed_seen[i] = true;
                w.ever_changed += 1;
            }
        }
    }

    // Track how far into the mapping non-zero data has ever reached.
    for i in (0..w.dwords).rev() {
        if snap[i] != 0 {
            let hw = w.high_water_dword.unwrap_or(0);
            if i > hw {
                w.high_water_dword = Some(i);
            }
            break;
        }
    }

    // Capture the earliest evidence of real data outside the suspected header, before a ring can
    // wrap or be drained over it. This snapshot is what the final hex dump prints.
    if w.first_data_snapshot.is_none() {
        let prefix = (CONTROL_PREFIX_BYTES / 4).min(w.dwords);
        if snap[prefix..].iter().any(|&v| v != 0) {
            w.first_data_snapshot = Some(snap.clone());
            w.first_data_at_ms = t;
            eprintln!(
                "[ring +{t:8.2}ms] res={}: FIRST DATA outside the first {CONTROL_PREFIX_BYTES} bytes — snapshot captured",
                w.resource_id
            );
        }
    }

    if changed.is_empty() {
        w.prev = snap;
        return;
    }
    w.changed_samples += 1;

    // Report the change as coalesced byte ranges, so a 4 KiB burst prints as one range rather than
    // a thousand indices.
    let ranges = coalesce(&changed);
    let range_text: Vec<String> = ranges
        .iter()
        .take(8)
        .map(|(a, b)| format!("{:#x}..{:#x}", a * 4, (b + 1) * 4))
        .collect();
    let more = if ranges.len() > 8 {
        format!(" (+{} more ranges)", ranges.len() - 8)
    } else {
        String::new()
    };
    eprintln!(
        "[ring +{t:8.2}ms] res={}: {} dwords changed in {} range(s): {}{}",
        w.resource_id,
        changed.len(),
        ranges.len(),
        range_text.join(" "),
        more
    );

    // For dwords inside the suspected control area, print the actual transition. This is the
    // evidence that would show a head/tail index advancing — or show that no such thing exists.
    let prefix_dwords = CONTROL_PREFIX_BYTES / 4;
    for &i in changed.iter().filter(|&&i| i < prefix_dwords).take(12) {
        eprintln!(
            "            ctrl dword @{:#06x}: {:#010x} -> {:#010x}",
            i * 4,
            w.prev[i],
            snap[i]
        );
    }

    w.prev = snap;
}

/// Coalesce a sorted list of indices into inclusive `(start, end)` ranges, so change reports stay
/// readable when a large contiguous region moves at once.
fn coalesce(indices: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &i in indices {
        match out.last_mut() {
            // Extend the current run if this index is adjacent to it.
            Some(last) if last.1 + 1 == i => last.1 = i,
            // Otherwise start a new run.
            _ => out.push((i, i)),
        }
    }
    out
}

/// Prints the end-of-session report: per-dword behaviour of the suspected control area, the
/// high-water marks, an annotated hex dump of the earliest captured data, and the doorbell totals.
///
/// Called from `vtest::serve_vtest` on a clean end of session. It takes one last sample first, so
/// the report describes the mapping's *final* state as well as its history.
pub fn final_report() {
    if !enabled() {
        return;
    }
    // One last look, so the statistics include whatever happened since the sampler's last pass.
    sample_once();

    let Some(registry) = WATCHED.get() else {
        eprintln!("--- VENUS RING DUMP: no blob was ever watched (the client created none) ---");
        return;
    };
    let mut guard = registry.lock().unwrap_or_else(|e| e.into_inner());

    eprintln!("\n============== VENUS RING DUMP FINAL REPORT ==============");
    for w in guard.iter_mut() {
        report_watched(w);
    }

    // The doorbell cross-check: socket-side traffic, to be divided by ring-side traffic by the
    // reader. Printed as raw counters, with the ratio left to the report rather than asserted here.
    eprintln!("\n--- socket-side traffic (for comparison with the ring byte counts above) ---");
    eprintln!(
        "  SUBMIT_CMD2 messages : {}",
        DOORBELL_MESSAGES.load(Ordering::Relaxed)
    );
    eprintln!(
        "  SUBMIT_CMD2 batches  : {}",
        DOORBELL_BATCHES.load(Ordering::Relaxed)
    );
    eprintln!(
        "  inline cmd dwords    : {} ({} bytes)",
        DOORBELL_CMD_DWORDS.load(Ordering::Relaxed),
        DOORBELL_CMD_DWORDS.load(Ordering::Relaxed) * 4
    );
    eprintln!("=========================================================\n");
}

/// The per-mapping half of [`final_report`].
fn report_watched(w: &mut Watched) {
    eprintln!(
        "\n--- res={} : {} bytes ({} dwords) ---",
        w.resource_id,
        w.dwords * 4,
        w.dwords
    );
    eprintln!("  samples with changes      : {}", w.changed_samples);
    eprintln!(
        "  distinct dwords ever moved: {} of {} ({:.2}% of the mapping)",
        w.ever_changed,
        w.dwords,
        100.0 * w.ever_changed as f64 / w.dwords as f64
    );
    match w.high_water_dword {
        Some(i) => eprintln!(
            "  highest non-zero dword    : {i} (byte {:#x} of {:#x})",
            i * 4,
            w.dwords * 4
        ),
        None => {
            eprintln!("  highest non-zero dword    : NONE — the mapping was zero at every sample")
        }
    }

    // Per-dword behaviour of the control candidates. Only dwords that ever moved are printed:
    // a dword that never changed is not evidence of anything and would bury the ones that are.
    eprintln!(
        "  control-candidate dwords that ever changed (offset: changes, min..max, monotonic):"
    );
    let mut any = false;
    for s in w.stats.iter() {
        if s.changes == 0 {
            continue;
        }
        any = true;
        eprintln!(
            "    @{:#08x}: {:4} changes, {:#010x}..{:#010x}, monotonic_up={}",
            s.byte_offset, s.changes, s.min, s.max, s.monotonic_up
        );
    }
    if !any {
        eprintln!("    (none — no dword in the suspected control area ever changed)");
    }

    // The hex evidence. Prefer the earliest-data snapshot (before a ring could wrap over itself);
    // fall back to the final state if data never appeared outside the header.
    match &w.first_data_snapshot {
        Some(snap) => {
            eprintln!(
                "\n  hex of the FIRST-DATA snapshot (captured at +{:.2}ms):",
                w.first_data_at_ms
            );
            dump_regions(snap);
        }
        None => {
            eprintln!(
                "\n  no data ever appeared outside the first {CONTROL_PREFIX_BYTES} bytes; hex of the FINAL state:"
            );
            let final_snap = w.prev.clone();
            dump_regions(&final_snap);
        }
    }
}

/// Prints the two regions worth reading out of a snapshot: the suspected control area verbatim, and
/// the first non-zero stretch beyond it.
fn dump_regions(snap: &[u32]) {
    let prefix = (CONTROL_PREFIX_BYTES / 4).min(snap.len());

    eprintln!("    [suspected control area — first {CONTROL_PREFIX_BYTES} bytes, verbatim]");
    dump_hex(snap, 0, prefix);

    // Find where real content starts beyond the header, and print a window around it.
    let Some(first) = (prefix..snap.len()).find(|&i| snap[i] != 0) else {
        eprintln!(
            "    [beyond the control area: ALL ZERO across {} dwords]",
            snap.len() - prefix
        );
        return;
    };
    // 128 dwords (512 bytes) is enough to see several command headers repeat if there are any, and
    // short enough that a human can actually read it.
    let end = (first + 128).min(snap.len());
    eprintln!(
        "    [first non-zero dword beyond the control area: index {first} (byte {:#x}); {} dwords shown]",
        first * 4,
        end - first
    );
    dump_hex(snap, first, end);
}

/// Prints `snap[from..to]` as 8 dwords per line, each line labelled with its byte offset. Little
/// endian on the wire; printed as the `u32` values the client wrote, matching how
/// `vtest::dump_submit_cmd2` prints an inline command stream so the two can be compared by eye.
fn dump_hex(snap: &[u32], from: usize, to: usize) {
    let mut i = from;
    while i < to {
        let end = (i + 8).min(to);
        let hex: Vec<String> = snap[i..end].iter().map(|d| format!("{d:08x}")).collect();
        eprintln!("      {:#08x}  {}", i * 4, hex.join(" "));
        i = end;
    }
}
