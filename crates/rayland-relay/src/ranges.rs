//! Merging changed byte ranges that are nearly adjacent, and the one safety rule that governs it.
//!
//! # Why this lives here rather than in either daemon
//! Both directions of the relay diff a blob against a baseline and ship the runs that differ, and
//! both hit the same problem: a byte-granular diff of a megabyte-scale buffer shatters into
//! thousands of one- and two-byte runs, and each run is a message with a lock, a serialisation and a
//! flush behind it. Both are message-rate-bound rather than bandwidth-bound, so both want to trade a
//! bounded number of re-shipped *unchanged* bytes for far fewer messages.
//!
//! The merge itself is four lines. What must not drift between the two sides is the **argument for
//! when it is legal**, which is why one copy lives in the crate they both already depend on rather
//! than one copy in each daemon:
//!
//! > **Re-shipping an unchanged byte is safe exactly when the sender's model of the receiver's copy
//! > is faithful for that byte** — i.e. when writing it lands a value the receiver already holds.
//! > A gap byte is by definition one where sender and baseline agree; if the baseline is a true
//! > model of the receiver, the write is idempotent. Where the receiver can change a byte *without
//! > telling the sender*, the baseline is stale, and re-shipping the gap **clobbers the receiver's
//! > authoritative copy with old news**.
//!
//! Each caller therefore owes an argument that the blob it passes a non-zero `gap` for has no
//! unreported writes on the far side, and each states it at the call site:
//!
//! - **S→C** (`rayland_s::apply::Applier::take_app_blob_writes`): the readback buffer is written by
//!   S's GPU and only read by C, so C never has bytes for S's model to be stale about.
//! - **C→S** (`rayland_c::blob_sync::messages_for_delta`): S reports every byte it writes back to C
//!   — `note_s_wrote` folds them into the baseline — *except* for resources S presents to its
//!   compositor, which S deliberately excludes from its return path. Those are exactly the blobs C
//!   must pass `gap == 0` for, and C knows them because C is the side that published their
//!   `BufferToken`s.
//!
//! `gap == 0` is inert by construction: the diffs that feed this never return adjacent ranges, so
//! nothing merges and the output equals the input.

/// Merge ascending, non-overlapping `(start, end)` ranges separated by at most `gap` unchanged bytes
/// into single ranges, re-including the bytes in between.
///
/// # Inputs / outputs
/// - `ranges`: half-open `[start, end)` ranges, **ascending and non-overlapping**. Both callers'
///   diffs produce exactly that; the function does not sort or validate, because a caller that hands
///   it anything else has a bug this cannot repair, and silently repairing it would hide the bug.
/// - `gap`: a range is merged into the previous one when its start is within `gap` bytes of that
///   range's end. `gap == 0` merges nothing (see the module docs).
/// - Returns the coalesced ranges, ascending and non-overlapping. Never returns more ranges than it
///   was given.
///
/// # Failure modes
/// None. `start - last.1` cannot underflow given ascending, disjoint inputs, and an empty input
/// yields an empty output.
pub fn coalesce_ranges(ranges: Vec<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        // Extend the open run if this range begins within `gap` unchanged bytes of its end; otherwise
        // start a fresh run.
        if let Some(last) = out.last_mut() {
            if start - last.1 <= gap {
                last.1 = end;
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::coalesce_ranges;

    #[test]
    fn merges_ranges_within_the_gap() {
        // [0,3) then [5,8): a 2-byte unchanged gap (3..5). With a threshold of 2 they merge, so the
        // gap bytes ride along in one run instead of splitting into two messages.
        assert_eq!(coalesce_ranges(vec![(0, 3), (5, 8)], 2), vec![(0, 8)]);
    }

    #[test]
    fn keeps_ranges_farther_apart_than_the_gap_split() {
        // Same 2-byte gap, threshold 1: it exceeds the threshold, so the runs stay split and no
        // unchanged bytes are re-shipped.
        assert_eq!(
            coalesce_ranges(vec![(0, 3), (5, 8)], 1),
            vec![(0, 3), (5, 8)]
        );
    }

    #[test]
    fn a_zero_gap_merges_nothing() {
        // The inert case both byte-granular paths rely on: adjacent-but-not-touching ranges stay
        // separate, so a caller that has not earned the safety argument keeps the strict grain.
        assert_eq!(
            coalesce_ranges(vec![(0, 3), (4, 8)], 0),
            vec![(0, 3), (4, 8)]
        );
    }

    #[test]
    fn chains_several_small_gaps_into_one() {
        // The real pattern on both paths: many tiny runs separated by tiny gaps collapse to one.
        assert_eq!(
            coalesce_ranges(vec![(3, 4), (7, 8), (11, 12)], 256),
            vec![(3, 12)]
        );
    }

    #[test]
    fn an_empty_input_yields_an_empty_output() {
        assert_eq!(coalesce_ranges(vec![], 256), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn a_run_of_exactly_gap_plus_one_unchanged_bytes_does_not_merge() {
        // The boundary, stated as its own case because off-by-one here silently changes how many
        // unchanged bytes cross the wire: gap 4 merges a 4-byte hole and splits a 5-byte one.
        assert_eq!(coalesce_ranges(vec![(0, 2), (6, 8)], 4), vec![(0, 8)]);
        assert_eq!(
            coalesce_ranges(vec![(0, 2), (7, 9)], 4),
            vec![(0, 2), (7, 9)]
        );
    }
}
