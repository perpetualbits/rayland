#!/usr/bin/env bash
#
# THE DEMO: a spinning icosahedron computed on C (apollo), rendered by S's GPU (dop561),
# and displayed live in a window on S's screen.
# =====================================================================================
#
# This is the thing Rayland exists to do, run end to end for a human to look at:
#   * `rayland-icosa-cpu` executes on apollo. Apollo does no rendering.
#   * Its Vulkan commands cross the network as *commands*, not pixels.
#   * dop561's GPU draws them, and dop561's compositor shows the result live.
#
# WHAT TO EXPECT
#   A 256x256 window titled by `rayland-s`, showing the icosahedron spinning as the frames
#   arrive. The window opens as soon as the first complete frame lands and keeps following
#   the render; it stays up after the run ends, so close it when you have seen enough.
#
#   Pacing is the compositor's, not the relay's: the window redraws on every frame callback
#   and shows whichever frame S last completed. Over a real network the render is slower
#   than the display, so frames repeat — the motion is the render's, sampled by the screen.
#
# WHY NOT vkcube
#   vkcube is the worst possible first demo: it is latency-bound in setup and, on this
#   machine, it selects the discrete NVIDIA GPU and provokes VK_ERROR_DEVICE_LOST on its
#   fifth submit (docs/DIARY.md, 2026-07-26). The icosahedron exercises the same relay
#   without either problem.
set -euo pipefail

C_HOST="${C_HOST:-apollo}"
S_IP="${S_IP:-192.168.1.192}"
PORT="${PORT:-9403}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
SOCK="/tmp/rl-icosa-demo.sock"
# The icosa fixtures render at `rayland_icosa_core::IMAGE_SIZE` = 256.
PRESENT_SIZE="${PRESENT_SIZE:-256x256}"

echo "### building (release: the app must be fast enough to look like motion) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p rayland-c -p rayland-s -p rayland-icosa-cpu

echo "### deploying C-side binaries to $C_HOST ###"
scp -q "$BIN/rayland-c" "$BIN/rayland-icosa-cpu" "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-icosa-cpu'

S_PID=""
# Kill only by exact PID — the local rayland-s by the PID captured here, the remote C-side by
# the PIDs it writes to /tmp. Never `pkill`/pattern: a pattern kill can match unrelated processes.
cleanup() {
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  ssh "$C_HOST" 'for p in /tmp/rayland-c.pid /tmp/rayland-app.pid; do kill "$(cat "$p" 2>/dev/null)" 2>/dev/null || true; done' 2>/dev/null || true
}
trap cleanup EXIT

echo "### starting S (this machine): GPU + compositor + the window ###"
# No RAYLAND_C1_NO_PRESENT here — that variable is what the *correctness* sweep sets, and it is
# exactly what has kept this fixture off the screen until now.
RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" RAYLAND_C1_PRESENT_SIZE="$PRESENT_SIZE" \
  "$BIN/rayland-s" &
S_PID=$!
sleep 3
kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s died on startup"; exit 1; }

echo "### starting C ($C_HOST): the application, which never touches a GPU ###"
# The fixture writes a PNG per frame and exits non-zero if it cannot; without this it dies on
# frame 0 having rendered exactly one frame, which looks like "the demo showed a still image".
ssh "$C_HOST" 'rm -rf /tmp/icosa-demo-out && mkdir -p /tmp/icosa-demo-out'
ssh "$C_HOST" "
  RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-demo.log 2>&1 &
  echo \$! > /tmp/rayland-c.pid
  sleep 3
  VN_DEBUG=vtest VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
  env -u VK_LOADER_DRIVERS_SELECT /tmp/rayland-icosa-cpu /tmp/icosa-demo-out >/dev/null 2>&1 &
  app_pid=\$!; echo \$app_pid > /tmp/rayland-app.pid
  wait \$app_pid || echo APP_EXIT_NONZERO
"
echo "### the application has finished; the window stays up until you close it ###"
wait "$S_PID" || true
