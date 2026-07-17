#!/usr/bin/env bash
#
# (c)1 Task 9 — the measurement sweep: what does remoting actually cost, as latency rises?
# ============================================================================================
#
# WHAT THIS ANSWERS
#   Task 8 proved an unmodified Vulkan app *can* render across a network. It said nothing about
#   what that costs. This script produces the numbers that turn (c)1 from a demo into evidence,
#   by running real workloads across a real link at four round-trip times and recording, per run:
#   bytes each way split by channel, round trips and the time actually spent blocked in them,
#   time to first frame, wall-clock, and whether the pixels were still bit-identical.
#
#   The three predictions under test are the design spec's S8.1, and each has a column here:
#     1. Steady state is BANDWIDTH-bound, not RTT-bound (Venus is asynchronous by design).
#        -> if true, wall-clock barely moves across the RTT columns for a given workload.
#     2. Startup is RTT-bound, but ONE-OFF.
#        -> if true, first_frame_us rises roughly linearly with RTT while wall-clock does not.
#     3. The return path is ~12x the command path (ring-findings S7 measured the reply arena).
#        -> read off s2c_total_bytes / c2s_total_bytes. This is a RATIO, so the two totals are
#           never summed anywhere.
#
# THE QUESTION THE ICOSA FIXTURES WERE BUILT TO ASK (docs/icosa-fixtures.md S11)
#   "Does (c)1's blob sync actually ship the megabyte every frame?" Fixture A writes 1 MiB into a
#   persistently-mapped HOST_COHERENT buffer every frame, with no flush and no interceptable API
#   call, for 120 frames. If nothing elides it, c2s_blob_sync_bytes should be ~120 MiB. The
#   fractal is a ZOOMING view — every texel changes every frame — so Task 5b's byte-granular diff
#   should find nothing to elide. If it elides anything substantial, that is a SURPRISING result
#   and must be understood before the diff is trusted elsewhere, not waved through.
#
# THE LINK (read this before believing any number here)
#   S = dop561, C = apollo. dop561 has TWO addresses and they are NOT equivalent:
#     192.168.1.192 -> dop561's WiFi.  Measured RTT from apollo: avg 11.8ms, max 91ms, mdev 26ms.
#     192.168.1.150 -> dop561's br0, a wired USB Ethernet adapter. RTT: avg 0.65ms, mdev 0.18ms.
#   Task 8 used the WiFi address (and passed 7/7 across it, which is a stronger result than it
#   sounds). This sweep uses the WIRED address, deliberately: a "0 ms RTT" cell is meaningless on
#   a link whose own jitter is +/-26 ms, and netem's 20 ms would be measuring the access point
#   rather than the protocol. The WiFi path is worth a row of its own (S_IP=192.168.1.192), as an
#   uncontrolled real-world link — but it is not the controlled floor the sweep needs.
#
# WHY NETEM IS FILTERED RATHER THAN APPLIED TO THE WHOLE INTERFACE
#   `tc qdisc add dev enp1s0 root netem delay 100ms` delays EVERYTHING apollo sends, including its
#   ssh replies to this script — apollo's route to dop561 is enp1s0 for both. That would slow the
#   orchestration, confuse the timeouts, and make a working machine look broken. Instead a `prio`
#   qdisc sends only UDP-to-S into a netem band, so the app's QUIC traffic is delayed and the ssh
#   control channel is not. The delay is applied on C's egress only, so it adds N ms to the RTT
#   (not 2N): a reply from S is not delayed on its way back.
#
# WHY THE BASELINE IS REGENERATED IN-RUN RATHER THAN REUSED
#   Each workload's native-on-S baseline is produced by this script, in this run, on this machine.
#   A baseline from another day is a baseline from another driver state. Compare against S-native
#   ONLY (spec S10.2): C's GPU is an AMD part and comparing against it would compare rasterisers
#   instead of transports. `docs/icosa-fixtures.md` S11 states the same rule for the fixtures.
#
# WHY `VN_DEBUG=no_abort` IS ABSENT (do not add it)
#   Mesa aborts the app ~3.5s after a ring stalls, and that abort is the stall detector: Task 6
#   found four faults on the first live run and `no_abort` would have turned each into a silent
#   hang. Fixture A's 120 frames give a stall far more chances to happen than refapp's single
#   frame ever did (docs/icosa-fixtures.md S11 says so explicitly). A hang here is a FINDING.
#
# EXCLUSIVE ACCESS (messagebox/README.md)
#   While this runs, dop561's GPU must be otherwise idle: a contended measurement is not a failed
#   one, it is a WRONG one, which is worse. Post SWEEP WINDOW OPEN before, CLOSED after.
#
# Usage:
#   scripts/c1-sweep.sh                       # full sweep: 3 workloads x 4 RTTs
#   RTTS="0 100" WORKLOADS=refapp scripts/c1-sweep.sh   # a subset, for a smoke test
#   S_IP=192.168.1.192 RTTS=0 scripts/c1-sweep.sh       # the uncontrolled WiFi row
set -euo pipefail

# ---- Configuration (override via environment) ----------------------------------------------
C_HOST="${C_HOST:-apollo}"                  # machine C, as an ssh destination
C_IFACE="${C_IFACE:-enp1s0}"                # C's interface that routes to S; netem is applied here
S_IP="${S_IP:-192.168.1.150}"               # S's WIRED address (see "THE LINK" above)
PORT="${PORT:-9401}"                        # QUIC (UDP) port S listens on
RTTS="${RTTS:-0 20 50 100}"                 # added one-way delay in ms == added RTT in ms
WORKLOADS="${WORKLOADS:-refapp icosa-cpu icosa-gpu}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-task9-target}"
BIN="$TARGET_DIR/release"                   # release: a debug fractal would measure rustc, not the relay
SOCK="/tmp/rl-c1.sock"                      # C-local vtest socket Mesa connects to (sun_path < 108)
OUT_DIR="${OUT_DIR:-/tmp/c1-sweep}"         # where results and the CSV land
CSV="$OUT_DIR/sweep.csv"

# The ssh key and options that work regardless of the gnome-keyring agent's state. The agent lists
# locked keys via `ssh-add -l` and then cannot sign with them, burning MaxAuthTries before ssh ever
# reaches the right key — so the key is named explicitly and the agent's opinion is excluded.
# (docs/icosa-fixtures.md S11 records the same trap; it recurs after every reboot or screen-lock.)
SSH_OPTS=(-i /home/roland/.ssh/keys.d/stationoost/id_ed25519 -o IdentitiesOnly=yes -o BatchMode=yes)
c_ssh() { ssh "${SSH_OPTS[@]}" "roland@${C_HOST}.localdomain" "$@"; }
c_scp() { scp -q "${SSH_OPTS[@]}" "$@"; }
C_DEST="roland@${C_HOST}.localdomain"

mkdir -p "$OUT_DIR"

# ---- Per-workload facts --------------------------------------------------------------------
# Each workload names the binary to run, whether its output argument is a FILE or a DIRECTORY, and
# how many frames it renders. Fixture A and B take a directory that MUST ALREADY EXIST — they exit
# 1 with "No such file or directory (os error 2)" otherwise, which once made 120 frames appear to
# take 0.48s and nearly got recorded as a measurement (docs/icosa-fixtures.md S11).
workload_bin()    { case "$1" in refapp) echo rayland-refapp;; icosa-cpu) echo rayland-icosa-cpu;; icosa-gpu) echo rayland-icosa-gpu;; esac; }
workload_kind()   { case "$1" in refapp) echo file;; *) echo dir;; esac; }
workload_frames() { case "$1" in refapp) echo 1;; *) echo 120;; esac; }

# ---- Cleanup: apollo must never be left crippled -------------------------------------------
# The plan is explicit: "ALWAYS clean up — do not leave apollo crippled". A netem qdisc left on the
# interface would silently delay every future run on this machine, including someone else's, and it
# would look like a network fault rather than our residue. This trap runs on success, on failure,
# and on Ctrl-C.
S_PID=""
cleanup() {
  local rc=$?
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  c_ssh "sudo -n tc qdisc del dev $C_IFACE root 2>/dev/null; pkill -x rayland-c; pkill -f '[/]tmp/rayland-refapp'; pkill -f '[/]tmp/rayland-icosa'" 2>/dev/null || true
  echo "### cleanup done: netem removed from $C_HOST:$C_IFACE, daemons killed ###"
  exit $rc
}
trap cleanup EXIT INT TERM

# ---- netem control -------------------------------------------------------------------------

# Remove any qdisc we added, restoring the interface's default.
netem_clear() { c_ssh "sudo -n tc qdisc del dev $C_IFACE root 2>/dev/null || true"; }

# Delay ONLY UDP traffic to S by $1 ms on C's egress. See the header for why this is filtered
# rather than applied at the root: an unfiltered netem also delays this script's own ssh.
netem_apply() {
  local ms="$1"
  netem_clear
  [ "$ms" = "0" ] && return 0
  c_ssh "
    set -e
    # prio gives three bands; band 3 (1:3) is where the filtered traffic goes.
    sudo -n tc qdisc add dev $C_IFACE root handle 1: prio
    # netem sits on band 3 only, so nothing else on this interface is touched.
    sudo -n tc qdisc add dev $C_IFACE parent 1:3 handle 30: netem delay ${ms}ms
    # Match UDP (ip protocol 17) destined for S. QUIC is UDP; ssh is TCP and stays in the fast bands.
    sudo -n tc filter add dev $C_IFACE protocol ip parent 1:0 prio 3 u32 \
      match ip dst $S_IP/32 match ip protocol 17 0xff flowid 1:3
  "
}

# Return the number of packets netem actually saw, or 0 if no netem is installed.
#
# This exists because `tc` succeeding does not prove the filter matched our traffic: a typo'd
# filter installs perfectly and delays nothing, and the sweep would then report "100 ms RTT" over
# an undelayed link — a wrong number that looks like a finding. Asserting the counter moved is the
# difference between believing tc and checking it.
netem_packets() {
  c_ssh "tc -s qdisc show dev $C_IFACE 2>/dev/null" \
    | awk '/netem/{f=1} f&&/Sent/{print $4; exit} END{if(!f) print 0}'
}

# ---- Metrics parsing -----------------------------------------------------------------------

# Print the C1METRICS line with the greatest elapsed_us.
#
# The totals are monotonic precisely so this rule is safe: the daemon is killed at the end of a
# run, and a SIGKILL landing mid-print truncates the final line. Taking the max rather than the
# last means a truncated tail costs nothing. (The idea is inherited from the throwaway spike that
# `crates/rayland-c/src/metrics.rs` replaced.)
metrics_max_line() {
  awk '/^C1METRICS /{
         e=0
         for (i=2;i<=NF;i++) { split($i,kv,"="); if (kv[1]=="elapsed_us") e=kv[2]+0 }
         if (e>=best) { best=e; line=$0 }
       }
       END { print line }' "$1"
}

# True if the log contains the daemon's authoritative end-of-session line.
metrics_is_final() { grep -q 'C1METRICS .*final=1' "$1" 2>/dev/null; }

# Wait (up to $1 seconds) for C's daemon to finish its session and exit.
#
# # Why this exists: without it the sweep measured the same cell at 60,319 and 10,091 bytes
# The application exiting does NOT mean the session is over — `rayland-c` is still relaying when the
# app's process is gone. Snapshotting its log at app-exit therefore samples a session in progress,
# and the faster the cell, the more traffic is missed. That produced a 6x spread on identical refapp
# runs which looked like a property of Venus and was really this harness reading too early. The
# monotonic-max rule makes a truncated *print* harmless; nothing but waiting makes a truncated
# *session* harmless.
#
# The daemon exits on its own once Mesa closes the vtest socket, printing `final=1`. If it has not
# exited within the timeout, that is a finding — a daemon that will not retire after its application
# is gone — and the caller marks the row rather than quietly using a partial line.
# `pgrep -x` matches the process NAME exactly, never the command line. That distinction is not
# pedantry: `pgrep -f /tmp/rayland-c` matches the argv of the very `bash -c` shell running the
# pgrep, so it reports "still running" forever and every row came back `daemon_did_not_retire`
# while the daemon had in fact exited cleanly. A detector that always fires is not a detector.
c_wait_for_daemon_exit() {
  local timeout="${1:-30}" waited=0
  while c_ssh "pgrep -x rayland-c >/dev/null" 2>/dev/null; do
    sleep 1
    waited=$((waited + 1))
    if [ "$waited" -ge "$timeout" ]; then
      return 1
    fi
  done
  return 0
}

# Pull one key=value out of a C1METRICS line; print empty if absent.
metric_get() {
  echo "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2; exit}'
}

# ---- Build ---------------------------------------------------------------------------------
# Release, because fixture A spends ~49 ms/frame in a CPU Mandelbrot and a debug build would make
# this a measurement of rustc's -O0 output rather than of the relay.
echo "### building (release): rayland-c, rayland-s, and the three workloads ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release \
  -p rayland-c -p rayland-s -p rayland-refapp -p rayland-icosa-cpu -p rayland-icosa-gpu

# ---- Deploy the C-side binaries ------------------------------------------------------------
# C needs no Rust toolchain and no GPU stack: both hosts are Ubuntu 26.04 / glibc 2.43, so
# S-built binaries run on C unchanged. Fixture A introduces no new runtime dependency on C — its
# `ldd` output is identical to refapp's five entries, everything else being statically linked with
# Vulkan arriving via dlopen (verified by the icosa session, messagebox 2026-07-16T2247).
echo "### deploying C-side binaries to $C_HOST ###"
BINS=(rayland-c)
for w in $WORKLOADS; do BINS+=("$(workload_bin "$w")"); done
c_scp "${BINS[@]/#/$BIN/}" "$C_DEST:/tmp/"
c_ssh "chmod +x ${BINS[*]/#//tmp/}"

# ---- Native-on-S baselines (one per workload, this run) -------------------------------------
echo "### native baselines on S (Intel GPU, no Venus in the environment) ###"
for w in $WORKLOADS; do
  bin="$(workload_bin "$w")"
  if [ "$(workload_kind "$w")" = file ]; then
    "$BIN/$bin" "$OUT_DIR/native-$w.png"
  else
    # The directory must exist before the fixture runs; see workload_frames' comment.
    rm -rf "$OUT_DIR/native-$w"; mkdir -p "$OUT_DIR/native-$w"
    "$BIN/$bin" "$OUT_DIR/native-$w"
  fi
  echo "  $w: native baseline done"
done

# ---- The sweep -----------------------------------------------------------------------------
echo "workload,rtt_ms,status,wall_s,app_connected_us,first_frame_us,first_frame_after_connect_us,round_trips,round_trip_wait_us,c2s_total_bytes,s2c_total_bytes,c2s_ring_bytes,c2s_inline_bytes,c2s_blob_sync_bytes,c2s_control_bytes,s2c_replies_bytes,s2c_blob_sync_bytes,netem_pkts" > "$CSV"

for rtt in $RTTS; do
  echo "### applying netem: +${rtt} ms RTT on $C_HOST:$C_IFACE (UDP to $S_IP only) ###"
  netem_apply "$rtt"

  for w in $WORKLOADS; do
    bin="$(workload_bin "$w")"
    kind="$(workload_kind "$w")"
    echo "--- cell: workload=$w rtt=${rtt}ms ---"

    # S's daemon, headless: RAYLAND_C1_NO_PRESENT skips the on-screen window, which would otherwise
    # block until closed. A fresh daemon per cell so no state leaks between cells.
    RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" \
      "$BIN/rayland-s" >"$OUT_DIR/s-$w-$rtt.log" 2>&1 &
    S_PID=$!
    sleep 3
    kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s exited early:"; cat "$OUT_DIR/s-$w-$rtt.log"; exit 1; }

    # On C: the metered relay daemon, then the UNMODIFIED application through stock Mesa's Venus.
    # All four client variables matter and THREE fail silently:
    #   VN_DEBUG=vtest                  - without it Mesa prefers virtgpu and never connects
    #   VN_PERF=no_*_feedback           - disables S->C shared status pages (c)1 does not relay
    #   VK_ICD_FILENAMES=...            - the Venus ICD manifest
    #   env -u VK_LOADER_DRIVERS_SELECT - a host *intel* filter would hide Venus
    start=$(date +%s.%N)
    set +e
    c_ssh "
      set -e
      rm -rf /tmp/out-$w /tmp/out-$w.png /tmp/rayland-c.log
      RAYLAND_C1_METRICS=1 RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK \
        nohup /tmp/rayland-c >/tmp/rayland-c.log 2>&1 &
      sleep 3
      $( [ "$kind" = dir ] && echo "mkdir -p /tmp/out-$w" )
      VN_DEBUG=vtest \
      VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
      VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
      VTEST_SOCKET_NAME=$SOCK \
      env -u VK_LOADER_DRIVERS_SELECT /tmp/$bin $( [ "$kind" = dir ] && echo "/tmp/out-$w" || echo "/tmp/out-$w.png" )
    " >"$OUT_DIR/c-$w-$rtt.log" 2>&1
    app_rc=$?
    set -e
    end=$(date +%s.%N)
    wall=$(echo "$end - $start" | bc)

    pkts="$(netem_packets)"

    # Let the session finish before reading its numbers. See c_wait_for_daemon_exit for the 6x
    # measurement error this prevents. Never kill the daemon before this: killing it *is* the
    # truncation we are trying to avoid.
    daemon_retired="yes"
    c_wait_for_daemon_exit 30 || daemon_retired="no"

    # Retrieve the daemon's metered stderr, now that it is complete.
    c_scp "$C_DEST:/tmp/rayland-c.log" "$OUT_DIR/cdaemon-$w-$rtt.log" 2>/dev/null || true
    line="$(metrics_max_line "$OUT_DIR/cdaemon-$w-$rtt.log" 2>/dev/null || true)"

    # `status` is built worst-first: a rendering result means nothing if the app crashed, and byte
    # counts mean nothing if the session was still running when they were read. The first true
    # condition wins and the rest are not consulted.
    status="ok"
    if [ "$app_rc" != "0" ]; then
      # A non-zero exit is a finding, not a nuisance: with the Mesa abort armed, a ring stall lands
      # here rather than hanging forever. Distinguish it from a rendering difference below.
      status="app_exit_$app_rc"
    elif [ "$daemon_retired" = "no" ]; then
      # The app is gone but the daemon would not retire: a real finding about the relay, and also a
      # reason to distrust this row's byte counts, since the session never ended.
      status="daemon_did_not_retire"
    elif ! metrics_is_final "$OUT_DIR/cdaemon-$w-$rtt.log"; then
      # No `final=1` line: the numbers came from a session that had not finished. Not a measurement.
      status="metrics_not_final"
    else
      # Bit-identity against the native-on-S baseline (spec S10.2).
      if [ "$kind" = file ]; then
        c_scp "$C_DEST:/tmp/out-$w.png" "$OUT_DIR/out-$w-$rtt.png" 2>/dev/null || true
        cmp -s "$OUT_DIR/out-$w-$rtt.png" "$OUT_DIR/native-$w.png" || status="DIFFERS"
      else
        rm -rf "$OUT_DIR/out-$w-$rtt"; mkdir -p "$OUT_DIR/out-$w-$rtt"
        c_scp -r "$C_DEST:/tmp/out-$w/." "$OUT_DIR/out-$w-$rtt/" 2>/dev/null || true
        n_got=$(ls "$OUT_DIR/out-$w-$rtt" | wc -l)
        if [ "$n_got" != "$(workload_frames "$w")" ]; then
          status="frames_${n_got}_of_$(workload_frames "$w")"
        elif ! diff -rq "$OUT_DIR/out-$w-$rtt" "$OUT_DIR/native-$w" >/dev/null 2>&1; then
          status="DIFFERS"
        fi
      fi
    fi

    # Tear the cell down before recording, so a slow kill cannot overlap the next cell's timing.
    kill "$S_PID" 2>/dev/null || true; wait "$S_PID" 2>/dev/null || true; S_PID=""
    # C's daemon has already retired by itself (that is what c_wait_for_daemon_exit established), so
    # this only reaps one that overstayed — the daemon_did_not_retire case, already recorded above.
    c_ssh "pkill -x rayland-c" 2>/dev/null || true

    echo "$w,$rtt,$status,$wall,$(metric_get "$line" app_connected_us),$(metric_get "$line" first_frame_us),$(metric_get "$line" first_frame_after_connect_us),$(metric_get "$line" round_trips),$(metric_get "$line" round_trip_wait_us),$(metric_get "$line" c2s_total_bytes),$(metric_get "$line" s2c_total_bytes),$(metric_get "$line" c2s_ring_bytes),$(metric_get "$line" c2s_inline_bytes),$(metric_get "$line" c2s_blob_sync_bytes),$(metric_get "$line" c2s_control_bytes),$(metric_get "$line" s2c_replies_bytes),$(metric_get "$line" s2c_blob_sync_bytes),$pkts" >> "$CSV"
    echo "  -> status=$status wall=${wall}s netem_pkts=$pkts"

    # If netem was supposed to be delaying us and saw nothing, the cell's RTT label is a lie. Say so
    # loudly rather than emitting a row that looks like a measurement.
    if [ "$rtt" != "0" ] && [ "${pkts:-0}" = "0" ]; then
      echo "  !! WARNING: netem saw 0 packets at rtt=${rtt}ms — the filter did not match our traffic."
      echo "  !! This row's RTT label is NOT trustworthy. Fix the filter before believing it."
    fi
  done
done

netem_clear
echo
echo "### sweep complete: $CSV ###"
column -s, -t < "$CSV"
