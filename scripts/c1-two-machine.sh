#!/usr/bin/env bash
#
# (c)1 Task 8 — two-machine bring-up: an unmodified Vulkan app renders across a network.
# ============================================================================================
#
# WHAT THIS PROVES
#   `rayland-refapp` — an ordinary offscreen Vulkan program with zero knowledge of Rayland —
#   runs on machine C (apollo), which HAS NO GPU IN USE. Its Vulkan command stream is captured
#   by stock Mesa's Venus ICD, relayed by `rayland-c` over a real QUIC connection to machine S
#   (dop561), replayed there on S's Intel GPU by `rayland-s`, and the finished frame is read
#   back to C. The claim (c)1 exists to test: rendering can cross a network as *language* — a
#   command stream — not as pixels.
#
# THE CORRECTNESS ASSERTION (spec S10.2 — read it before "fixing" the comparison)
#   We compare the app's output run-through-Venus against `rayland-refapp` run NATIVELY ON S.
#   Both render on S's *Intel* GPU, so the result must be BIT-IDENTICAL. Do NOT compare against
#   the app run natively on C: C's GPU is AMD, a different rasteriser, and that comparison is
#   meaningless. The whole point is that only the *transport* changed, not the renderer.
#
# TOPOLOGY
#   S = dop561 (this host by default): Intel GPU, runs `rayland-s`. Also the build host.
#   C = apollo: x86_64, AMD GPU UNUSED, no Wayland needed. Runs `rayland-c` + the app.
#   Both are Ubuntu 26.04 / glibc 2.43, so S-built binaries run on C unchanged — no toolchain
#   is needed on C. C needs only Mesa's Venus ICD (stock) and the two copied binaries.
#
# WHY `VN_DEBUG=no_abort` IS DELIBERATELY ABSENT (do not add it)
#   Mesa aborts the app ~3.5s after a ring stalls. That abort is our stall detector: (c)1 Task 6
#   found FOUR faults on the first live run, and `no_abort` would have turned every one into a
#   silent hang. The spec keeps this crutch UNLICENSED until C's progress timeout is trusted.
#   Run with the abort armed; if the app hangs, that is a finding, not a nuisance.
#
# Usage:  scripts/c1-two-machine.sh          # build, deploy, run, compare, clean up
set -euo pipefail

# ---- Configuration (override via environment) ----------------------------------------------
C_HOST="${C_HOST:-apollo}"                 # machine C: where the app runs (ssh name)
S_IP="${S_IP:-192.168.1.192}"              # machine S's LAN address, as C must dial it
PORT="${PORT:-9401}"                       # QUIC (UDP) port S listens on
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"   # isolated so a co-tenant build is untouched
BIN="$TARGET_DIR/debug"
SOCK="/tmp/rl-c1.sock"                      # C-local vtest socket Mesa connects to (sun_path < 108)

# ---- Build the three binaries on S (only these crates; none link an icosa fixture) ---------
echo "### building rayland-c, rayland-refapp (for C) and rayland-s (for S) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p rayland-c -p rayland-refapp -p rayland-s

# ---- Deploy the two C-side binaries to apollo (C needs no toolchain, no GPU stack) ---------
echo "### copying the C-side binaries to $C_HOST ###"
scp -q "$BIN/rayland-c" "$BIN/rayland-refapp" "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-refapp'

# ---- Native baseline on S (Intel GPU, no Venus environment) --------------------------------
echo "### native baseline on S ###"
"$BIN/rayland-refapp" /tmp/native.png

# ---- Cleanup trap: never leave a daemon running on either machine --------------------------
S_PID=""
cleanup() {
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  ssh "$C_HOST" 'pkill -f /tmp/rayland-c; pkill -f /tmp/rayland-refapp' 2>/dev/null || true
}
trap cleanup EXIT

# ---- Start S's daemon (headless: RAYLAND_C1_NO_PRESENT skips the on-screen window) ----------
echo "### starting rayland-s on S (0.0.0.0:$PORT) ###"
RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >/tmp/rayland-s.log 2>&1 &
S_PID=$!
sleep 3
kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s exited early:"; cat /tmp/rayland-s.log; exit 1; }

# ---- On C: start rayland-c (dial S), then run the UNMODIFIED app through Venus --------------
#   All four client variables matter, and THREE of them fail silently:
#     VN_DEBUG=vtest            - without it Mesa prefers virtgpu and never connects (silent)
#     VN_PERF=no_*_feedback     - disables the S->C shared status pages (c)1 does not yet relay
#     VK_ICD_FILENAMES=...      - the Venus ICD manifest
#     env -u VK_LOADER_DRIVERS_SELECT - a host *intel* filter would hide Venus (silent)
echo "### running the unmodified refapp on C, through the relay ###"
ssh "$C_HOST" "
  set -e
  RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c.log 2>&1 &
  sleep 3
  rm -f /tmp/out.png
  VN_DEBUG=vtest \
  VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
  VTEST_SOCKET_NAME=$SOCK \
  env -u VK_LOADER_DRIVERS_SELECT /tmp/rayland-refapp /tmp/out.png
"

# ---- Retrieve C's output and assert bit-identity against S's native baseline ---------------
echo "### comparing ###"
scp -q "$C_HOST:/tmp/out.png" /tmp/out.png
if cmp -s /tmp/out.png /tmp/native.png; then
  echo "PASS: the app rendered on C's behalf on S's GPU across the network,"
  echo "      and the result is BIT-IDENTICAL to running it natively on S."
else
  echo "FAIL: the relayed frame differs from the native baseline. See /tmp/out.png vs /tmp/native.png."
  exit 1
fi
