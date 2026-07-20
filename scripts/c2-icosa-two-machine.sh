#!/usr/bin/env bash
#
# (c)2 — the readback-completion gate, proven over a real network.
# ============================================================================================
#
# WHAT THIS PROVES
#   `rayland-icosa-cpu` (120 frames, a spinning icosahedron textured with a per-frame CPU fractal
#   written into mapped HOST_COHERENT memory) runs on C (apollo) through `rayland-c`, is replayed
#   on S (dop561) by `rayland-s`, and read back. Before the readback-completion gate, ~2/120 frames
#   came back as the WHOLE PREVIOUS frame over a real link (0/120 on loopback) — a readback-delivery
#   lag on S, not a forward relay race (docs/design/2026-07-19-c2-true-remote-mapped-sync.md). After
#   the gate, every frame must match native-on-S across many runs.
#
# CORRECTNESS ASSERTION
#   Compare each relayed frame against `rayland-icosa-cpu` run NATIVELY ON S (same Intel GPU), so
#   only the transport differs and every frame must be bit-identical. Do NOT compare against the app
#   run on C (AMD GPU, a different rasteriser).
#
# WHY no VN_DEBUG=no_abort (do not add it): Mesa's ~3.5s stall-abort is the stall detector.
#
# Usage:  scripts/c2-icosa-two-machine.sh [RUNS]     # default 10 runs; exits non-zero on any stale frame
set -euo pipefail

C_HOST="${C_HOST:-apollo}"
S_IP="${S_IP:-192.168.1.192}"
PORT="${PORT:-9402}"
RUNS="${1:-10}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
SOCK="/tmp/rl-c2-icosa.sock"

echo "### building rayland-c, rayland-s, rayland-icosa-cpu (release; the app must be fast) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p rayland-c -p rayland-s -p rayland-icosa-cpu

echo "### native baseline on S (Intel GPU, no Venus) ###"
rm -rf /tmp/icosa-native && mkdir -p /tmp/icosa-native
"$BIN/rayland-icosa-cpu" /tmp/icosa-native >/dev/null
echo "native frames: $(ls /tmp/icosa-native/frame_*.png | wc -l)"

echo "### deploy C-side binaries to $C_HOST ###"
scp -q "$BIN/rayland-c" "$BIN/rayland-icosa-cpu" "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-icosa-cpu'

S_PID=""
cleanup() { [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true; ssh "$C_HOST" 'pkill -f /tmp/rayland-c; pkill -f /tmp/rayland-icosa-cpu' 2>/dev/null || true; }
trap cleanup EXIT

total_stale=0
for run in $(seq 1 "$RUNS"); do
  ssh "$C_HOST" 'rm -rf /tmp/icosa-relay; mkdir -p /tmp/icosa-relay'
  RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"/tmp/rayland-s-c2-$run.log" 2>&1 &
  S_PID=$!; sleep 3
  kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s died:"; cat "/tmp/rayland-s-c2-$run.log"; exit 1; }
  ssh "$C_HOST" "
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-icosa.log 2>&1 &
    sleep 3
    VN_DEBUG=vtest VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT /tmp/rayland-icosa-cpu /tmp/icosa-relay >/dev/null 2>&1 || echo APP_EXIT_NONZERO
  "
  sleep 1
  rm -rf /tmp/icosa-relay && scp -q -r "$C_HOST:/tmp/icosa-relay" /tmp/icosa-relay
  kill "$S_PID" 2>/dev/null || true; S_PID=""
  stale=0
  for f in /tmp/icosa-native/frame_*.png; do
    b=$(basename "$f")
    cmp -s "$f" "/tmp/icosa-relay/$b" 2>/dev/null || stale=$((stale + 1))
  done
  echo "run $run/$RUNS: $stale stale frame(s)"
  total_stale=$((total_stale + stale))
done

echo "TOTAL stale frames over $RUNS runs: $total_stale"
[ "$total_stale" -eq 0 ] || { echo "FAIL: stale frames remain — the gate did not fix it (see docs/design §9)"; exit 1; }
echo "PASS: 0 stale frames over $RUNS runs"
