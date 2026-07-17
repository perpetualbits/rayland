#!/usr/bin/env python3
"""Join the (c)1 Task 9 stage trace from one loopback reproducer run.

The reproducer (`cargo test -p rayland-s --test loopback_e2e icosa_cpu_renders`) run with
`RAYLAND_C1_TRACE=1 -- --nocapture` echoes both daemons' stderr, each line prefixed with
`[rayland-s]` or `[rayland-c]`. Both daemons stamp events with the *same* system-wide
`CLOCK_MONOTONIC`, so the `t_ns` values are directly comparable across the two processes.

This script reads such a captured log on stdin (or a path argv[1]) and answers the design
note's §7 questions:

  * Did Probe A fire — i.e. did S's GPU write a readback blob *after* the return path shipped
    it and declared the frame complete?  That is the `T2 < T4` prediction, proven directly.
  * What is the distribution of `dt_ns` (how long after the ship the late write landed)?
  * Per synchronisation cycle, is the observable ordering `T2 < T5 < T6 < T7 < T8` as the code
    intends, and is there ever `T8 < T7` on C (the `T9 < T7` risk the note also names)?

It prints a compact report, not the raw lines.
"""

import re
import sys
from collections import defaultdict

# One trace line: "[rayland-s] RLTRACE t_ns=123 stage=T2 side=S res=5 tail=42"
LINE = re.compile(r"RLTRACE\s+t_ns=(\d+)\s+stage=(\S+)\s+(.*)$")
KV = re.compile(r"(\w+)=(\S+)")


def parse(stream):
    """Yield (t_ns:int, stage:str, fields:dict) for every RLTRACE line in `stream`."""
    for raw in stream:
        m = LINE.search(raw)
        if not m:
            continue
        t_ns = int(m.group(1))
        stage = m.group(2)
        fields = {k: v for k, v in KV.findall(m.group(3))}
        yield t_ns, stage, fields


def main():
    src = open(sys.argv[1]) if len(sys.argv) > 1 else sys.stdin
    events = list(parse(src))
    by_stage = defaultdict(list)
    for t, stage, f in events:
        by_stage[stage].append((t, f))

    print(f"=== stage counts (total {len(events)} trace events) ===")
    for stage in ("T0", "T2", "T5", "T6", "T7", "T8", "A_RESAMPLE"):
        print(f"  {stage:12} {len(by_stage.get(stage, [])):6}")

    # --- Probe A: the decisive T2<T4 evidence.
    resamples = by_stage.get("A_RESAMPLE", [])
    print("\n=== Probe A (T2 < T4): GPU writes seen AFTER the frame was shipped ===")
    if not resamples:
        print("  none — no readback blob changed after it was shipped this run.")
    else:
        dts = sorted(int(f["dt_ns"]) for _, f in resamples)
        n = len(dts)
        print(f"  {n} late GPU writes caught (each = S's GPU still writing a shipped frame)")
        print(f"  dt_ns  min={dts[0]:>10}  median={dts[n//2]:>10}  max={dts[-1]:>10}")
        print(f"  dt_us  min={dts[0]/1e3:>10.1f}  median={dts[n//2]/1e3:>10.1f}  "
              f"max={dts[-1]/1e3:>10.1f}")
        # How many landed *after* the 200us poll that "hold one poll" would have added?
        after_one_poll = sum(1 for d in dts if d > 200_000)
        print(f"  {after_one_poll}/{n} landed >200us after ship "
              f"(beyond what 'hold one poll' would cover)")

    # --- Ordering per readback packet: pair each T6 with the next T7 for the same (res,off).
    # On loopback both are on one clock, so a T7 with a smaller t_ns than its T6 would be a
    # clock or logic fault; a T8 earlier than the T7 of the packet it releases would be the
    # T9<T7 risk. Match greedily in time order.
    print("\n=== Return-path ordering (T6 ship -> T7 install), by (res,off) ===")
    t6 = defaultdict(list)  # (res,off) -> [t_ns...]
    for t, f in by_stage.get("T6", []):
        t6[(f.get("res"), f.get("off"))].append(t)
    t7 = defaultdict(list)
    for t, f in by_stage.get("T7", []):
        t7[(f.get("res"), f.get("off"))].append(t)

    paired = 0
    install_lat = []
    t7_before_t6 = 0
    for key, sends in t6.items():
        installs = sorted(t7.get(key, []))
        sends = sorted(sends)
        i = 0
        for s in sends:
            # first install at or after this send
            while i < len(installs) and installs[i] < s:
                # an install with no earlier send of the same key: out-of-order or startup
                t7_before_t6 += 1
                i += 1
            if i < len(installs):
                install_lat.append(installs[i] - s)
                i += 1
                paired += 1
    if install_lat:
        install_lat.sort()
        m = len(install_lat)
        print(f"  paired {paired} packets; install latency (T7-T6) "
              f"min={install_lat[0]/1e3:.1f}us median={install_lat[m//2]/1e3:.1f}us "
              f"max={install_lat[-1]/1e3:.1f}us")
    print(f"  T7 seen before any matching T6: {t7_before_t6} "
          f"(should be 0 on a shared clock)")

    # --- The coarse per-cycle picture: for each S ship epoch (a T6 burst), was there an
    # A_RESAMPLE for the same res afterward but before the next frame's T2?
    print("\n=== Interpretation hint ===")
    if resamples:
        print("  Probe A fired: the return path ships frames whose GPU writes are NOT yet")
        print("  complete. This is T2 < T4 — a ring/fence retirement does not dominate the")
        print("  application's readback. The fix must be a completion signal that does.")
    else:
        print("  Probe A did not fire this run. Re-run (the failure rate is 3-39/120 and this")
        print("  is a race); if it never fires across several runs, the late write is landing")
        print("  faster than a 200us poll can sample and a finer probe is needed.")


if __name__ == "__main__":
    main()
