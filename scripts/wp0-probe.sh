#!/usr/bin/env bash
#
# Run the WP0 toolkit probe (winit + wgpu) through Rayland's Wayland proxy.
# =============================================================================================
#
# WHAT THIS IS FOR
#   `vkcube` is bare Vulkan over **libwayland**, and unusually undemanding: a window, some GPU
#   images, nothing else. A normal application is not like that. `tools/wgpu-window-probe` is the
#   smallest possible `winit` + `wgpu` client — the same stack `solarsim` uses, and therefore
#   `smithay-client-toolkit` over the **pure-Rust `wayland-client`**, a second client
#   implementation entirely against a proxy that had only ever faced the first one.
#
#   The probe exists to separate "the toolkit cannot work through Rayland" from "this particular
#   application cannot". Run it after any change to the proxy's advertised globals.
#
# WHY THIS DEFAULTS TO LOOPBACK, unlike scripts/wp0-vkcube-two-machine.sh
#   The question the probe answers — which services the toolkit asks the proxy for — is decided
#   entirely by what `rayland-c` advertises, which is identical whether S is across a network or
#   on localhost. Loopback is therefore sufficient AND always available; set C_HOST and S_ADDR to
#   run it split across two machines when the presentation path is what is under test.
#
#   It was written on a day apollo was unreachable, and the measurements it produced were taken
#   on loopback. That is stated rather than implied.
#
# WHY HEADLESS WESTON ON S
#   Same reason as scripts/wp0-soak.sh: a compositor emits frame callbacks only for surfaces it
#   composites, so a desktop that blanks or locks looks exactly like a stalled application. If a
#   headless weston is already running on $WESTON_SOCKET this reuses it; otherwise it starts one.
#   `--idle-time=0` is not optional — weston stops compositing after 300 s without it.
#
# WHAT SUCCESS AND FAILURE LOOK LIKE
#   The probe prints one line per stage (`creating window`, `requesting adapter`, ...) *before*
#   attempting it, so when it dies the last line names the step that killed it. Exit 0 means it
#   presented its frames. Any other exit is the finding, and the interesting part is the last
#   stage line plus the panic text.
#
# Usage:  scripts/wp0-probe.sh                     # loopback
#         C_HOST=milkv scripts/wp0-probe.sh        # app + rayland-c on another machine
set -uo pipefail

C_HOST="${C_HOST:-}"                        # empty = loopback (everything on this machine)
PORT="${PORT:-9413}"
SECONDS_TO_RUN="${SECONDS_TO_RUN:-45}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/debug"
PROBE="${PROBE:-$PWD/tools/wgpu-window-probe/target/release/wgpu-window-probe}"
SOCK=/tmp/rl-probe.sock
WL_SOCK=/tmp/rl-probe-wl.sock
WESTON_SOCKET="${WESTON_SOCKET:-wl-soak1}"
OUT="${OUT:-/tmp/wp0-probe-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"

if [ -n "$C_HOST" ]; then
  echo "ERROR: split-machine mode is declared but unverified — it has never been run." >&2
  echo "Remove this guard once it has, rather than trusting an untested path." >&2
  exit 1
fi
S_ADDR="127.0.0.1:$PORT"

echo "### building rayland-c, rayland-s and the probe ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build -q -p rayland-c -p rayland-s || exit 1
( cd tools/wgpu-window-probe && cargo build -q --release ) || exit 1

if ! pgrep -f "weston --backend=headless.*--socket=$WESTON_SOCKET" >/dev/null; then
  echo "### starting headless weston on $WESTON_SOCKET (Mesa/Intel, GL renderer) ###"
  rm -f "$XDG_RUNTIME_DIR/$WESTON_SOCKET" "$XDG_RUNTIME_DIR/$WESTON_SOCKET.lock"
  __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json \
    setsid weston --backend=headless --renderer=gl --width=1280 --height=1024 \
    --socket="$WESTON_SOCKET" --idle-time=0 --no-config > "$OUT/weston.log" 2>&1 &
  sleep 6
fi

S_PID=""; C_PID=""
cleanup() { [ -n "$C_PID" ] && kill "$C_PID" 2>/dev/null; [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null; true; }
trap cleanup EXIT

env WAYLAND_DISPLAY="$WESTON_SOCKET" RAYLAND_S_EVENT_LOG=1 RAYLAND_C1_NO_PRESENT=1 \
  RAYLAND_C1_S_LISTEN="$S_ADDR" "$BIN/rayland-s" > "$OUT/rayland-s.log" 2>&1 &
S_PID=$!; sleep 3

rm -f "$SOCK" "$WL_SOCK"
env RAYLAND_WP_LOG=1 RAYLAND_C1_S_ADDR="$S_ADDR" RAYLAND_C1_SOCKET="$SOCK" \
  RAYLAND_C1_WAYLAND_DISPLAY="$WL_SOCK" "$BIN/rayland-c" > "$OUT/rayland-c.log" 2>&1 &
C_PID=$!; sleep 3

# VN_DEBUG=vtest and the ICD/socket variables point Mesa's Venus at rayland-c rather than at a real
# GPU; `env -u VK_LOADER_DRIVERS_SELECT` because a host *intel* filter would hide Venus silently.
echo "### running the probe through the proxy ###"
env WAYLAND_DISPLAY="$WL_SOCK" WGPU_BACKEND=vulkan VN_DEBUG=vtest \
  VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME="$SOCK" \
  env -u VK_LOADER_DRIVERS_SELECT timeout "$SECONDS_TO_RUN" "$PROBE" > "$OUT/probe.log" 2>&1
status=$?

echo
echo "############ probe outcome (exit $status) ############"
grep -v '^\[' "$OUT/probe.log" | tail -8
echo
echo "############ what the proxy saw ############"
grep -E 'application connected|bound global|intercept|forward obj' "$OUT/rayland-c.log" | head -12
echo
echo "logs: $OUT"
[ "$status" -eq 0 ] && echo "PASS: the toolkit stack presented through Rayland." \
                    || echo "FAIL: see the last stage line above — that is the step that killed it."
