#!/usr/bin/env bash
#
# WP0 Task 4.3/4.5 — two-machine bring-up: an unmodified app's OWN WINDOW on S's screen.
# ============================================================================================
#
# WHAT THIS RUNS, AND HOW IT DIFFERS FROM `c1-two-machine.sh`
#   That script proves the *command relay*: an offscreen app renders on S's GPU and its pixels
#   come back to C, bit-identical. Nothing is displayed, and S never sees the app's Wayland
#   protocol at all.
#
#   This script proves the *presentation* half. `vkcube` on C (apollo) is an ordinary Vulkan
#   program that presents through Wayland. It connects to `rayland-c`'s Wayland **proxy**
#   instead of a compositor; the proxy forwards every request to S, where `rayland-s` replays
#   the session against S's REAL compositor — so the window that appears on dop561's screen is
#   the application's own window, not a picture of it.
#
# THE ONE THING THAT CANNOT CROSS, AND WHAT REPLACES IT
#   The app's swapchain `wl_buffer` names a dma-buf FILE DESCRIPTOR, and a file descriptor does
#   not survive a network. So C drops it and sends a `BufferToken` naming the S-side resource
#   the command relay already rendered into. S then originates -- does NOT replay -- three
#   requests against the dma-buf it exported for that resource at creation:
#       create_params  ->  add(fd, plane 0, offset, stride, modifier_hi, modifier_lo)
#                      ->  create_immed(new_id, width, height, format, flags)
#   `offset` and `stride` come from the token, never from `width x bpp`: a wrong stride garbles
#   the image instead of failing, so it is carried rather than recomputed on the machine that
#   never saw the image. NO PIXELS CROSS THE NETWORK -- only the name of where they already are.
#
# WHAT SUCCESS LOOKS LIKE (and what it does NOT claim)
#   The bar for 4.3 is in S's log: the three synthesized requests issued for at least one token,
#   NO protocol error from S's compositor, and the app-side wl_buffer id mapped. If the cube
#   additionally appears on dop561's screen, that is 4.5 reached -- report it separately, and
#   not on the strength of one run.
#   This script does NOT claim the presentation is correctly paced or tear-free. The app's
#   `commit` replays as soon as it arrives, with no wait on the (c)2 completion signal; frames
#   may be early or torn. That gating is a separate task ON PURPOSE, because shipping it here
#   would make any failure ambiguous between "the token path is wrong" and "the gating is wrong".
#
# *** vkcube MUST RUN WITH `--gpu_number 0` -- THIS IS NOT OPTIONAL ***
#   vkcube enumerates GPUs and picks the DISCRETE one by default. On dop561 that is an NVIDIA
#   RTX A500, and the real `vkQueueSubmit` on it returns VK_ERROR_DEVICE_LOST -- 7 of 14 runs,
#   against 0 of 10 on the Intel iGPU. Venus surfaces that as a generic "%s resulted in CS
#   error" with no log of its own, because it reports device loss through a branch that only
#   runs when flags == 0x0. THIS COST THE PROJECT THREE DAYS and was never a Rayland bug.
#   `--gpu_number 0` selects the Intel iGPU and avoids it entirely. See DIARY.md, 2026-07-26.
#
# WHY `VN_DEBUG=no_abort` IS DELIBERATELY ABSENT (do not add it)
#   Mesa aborts the app ~3.5 s after a ring stalls, and that abort is our stall detector. With
#   `no_abort` every stall becomes a silent hang instead of a finding. Run with it armed.
#
# TOPOLOGY
#   S = dop561 (this host): Intel GPU, a real Wayland compositor, runs `rayland-s`. Build host.
#   C = apollo: x86_64, GPU unused, no compositor of its own. Runs `rayland-c` + `vkcube`.
#   Both are Ubuntu 26.04, so S-built binaries run on C unchanged -- C needs no toolchain. C
#   needs stock Mesa's Venus ICD, libvulkan and libwayland-client, all of which it has; even
#   `vkcube` is copied from S, since it dlopens everything and links only libc/libm.
#
# PROCESS HYGIENE
#   Every process this script starts is killed BY THE PID IT CAPTURED, never by name pattern.
#   A pattern kill on a shared machine is how you take out somebody else's editor.
#
# Usage:  scripts/wp0-vkcube-two-machine.sh            # build, deploy, run, report
#         SECONDS_TO_RUN=20 scripts/wp0-vkcube-two-machine.sh
set -euo pipefail

# ---- Configuration (override via environment) ----------------------------------------------
C_HOST="${C_HOST:-apollo}"                 # machine C: where the app runs (ssh name)
# Machine S's LAN address, as C must dial it. DERIVED rather than hardcoded: `c1-two-machine.sh`
# hardcodes 192.168.1.192, which is no longer dop561's address, so that script fails on a DHCP
# lease change with a confusing timeout rather than an obvious error. Asking the routing table
# which source address this host would use to reach C cannot go stale.
C_IP="$(getent ahostsv4 "${C_HOST:-apollo}" | awk '{print $1; exit}')"
S_IP="${S_IP:-$(ip -4 route get "$C_IP" | grep -oP 'src \K[\d.]+')}"
PORT="${PORT:-9403}"                       # QUIC (UDP) port S listens on
SECONDS_TO_RUN="${SECONDS_TO_RUN:-15}"     # how long to let vkcube animate before stopping it
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/debug"
SOCK="/tmp/rl-wp0.sock"                    # C-local vtest socket Mesa connects to (sun_path < 108)
WL_SOCK="/tmp/rl-wp0-wayland.sock"         # C-local Wayland socket the app connects to instead of a compositor
S_LOG="/tmp/rayland-s-wp0.log"

# ---- Build (S builds both sides; C needs no toolchain) -------------------------------------
echo "### building rayland-c (for C) and rayland-s (for S) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build -p rayland-c -p rayland-s

# ---- Deploy to C: our daemon, plus vkcube itself -------------------------------------------
# vkcube links only libc/libm and dlopens libvulkan/libwayland at runtime, so the binary copies
# across unchanged rather than needing vulkan-tools installed on C.
echo "### copying rayland-c and vkcube to $C_HOST ###"
scp -q "$BIN/rayland-c" /usr/bin/vkcube "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/vkcube'

# ---- Cleanup: only PIDs we captured ourselves ----------------------------------------------
S_PID=""
C_PID=""
APP_PID=""
cleanup() {
  # C-side first, so the app stops driving the relay before S goes away.
  [ -n "$APP_PID" ] && ssh "$C_HOST" "kill $APP_PID 2>/dev/null" || true
  [ -n "$C_PID" ]   && ssh "$C_HOST" "kill $C_PID 2>/dev/null"   || true
  [ -n "$S_PID" ]   && kill "$S_PID" 2>/dev/null                 || true
  ssh "$C_HOST" "rm -f $SOCK $WL_SOCK" 2>/dev/null || true
}
trap cleanup EXIT

# ---- Start S's daemon ----------------------------------------------------------------------
# RAYLAND_C1_NO_PRESENT disables the (c)1 *readback* window. That path is not retired -- it is
# how offscreen fixtures are shown -- but for this run it would put a second, unrelated window
# on screen and make "did the app's window appear?" ambiguous. The WP0 replay's window is the
# one under test, and it does not go through that path at all.
echo "### starting rayland-s on S (0.0.0.0:$PORT), WP0 replay armed ###"
RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"$S_LOG" 2>&1 &
S_PID=$!
sleep 3
kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s exited early:"; cat "$S_LOG"; exit 1; }

# ---- On C: the daemon (with the Wayland proxy armed), then the unmodified app ---------------
#   RAYLAND_C1_WAYLAND_DISPLAY is what turns the proxy on. Without it rayland-c behaves exactly
#   as it did before WP0 and the app has no compositor to talk to at all.
#
#   The Venus client variables, three of which fail SILENTLY if omitted:
#     VN_DEBUG=vtest            - without it Mesa prefers virtgpu and never connects (silent)
#     VN_PERF=no_fence_feedback - LOAD-BEARING: the (c)2 completion barrier works by spotting the
#                                 app's vkGetFenceStatus reply reading VK_SUCCESS, and fence
#                                 feedback removes that poll. With it on: exit 134, zero frames,
#                                 every time. The other three feedbacks are off because their
#                                 one observed failure is unexplained, not because they are known
#                                 bad -- see DIARY.md 2026-07-27.
#     VK_ICD_FILENAMES=...      - the Venus ICD manifest
#     env -u VK_LOADER_DRIVERS_SELECT - a host *intel* filter would hide Venus (silent)
echo "### starting rayland-c on $C_HOST (proxy at $WL_SOCK) ###"
C_PID=$(ssh "$C_HOST" "
  rm -f $SOCK $WL_SOCK
  RAYLAND_C1_S_ADDR=$S_IP:$PORT \
  RAYLAND_C1_SOCKET=$SOCK \
  RAYLAND_C1_WAYLAND_DISPLAY=$WL_SOCK \
  nohup /tmp/rayland-c >/tmp/rayland-c-wp0.log 2>&1 &
  echo \$!
")
sleep 3

echo "### running vkcube on $C_HOST through the proxy for ${SECONDS_TO_RUN}s ###"
APP_PID=$(ssh "$C_HOST" "
  export XDG_RUNTIME_DIR=\${XDG_RUNTIME_DIR:-/run/user/\$(id -u)}
  WAYLAND_DISPLAY=$WL_SOCK \
  VN_DEBUG=vtest \
  VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
  VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
  VTEST_SOCKET_NAME=$SOCK \
  env -u VK_LOADER_DRIVERS_SELECT nohup /tmp/vkcube --gpu_number 0 >/tmp/vkcube.log 2>&1 &
  echo \$!
")
sleep "$SECONDS_TO_RUN"

# ---- Report: the 4.3 bar is in S's log ------------------------------------------------------
echo
echo "############ S-side WP0 log ############"
grep -E 'WP0|4\.3' "$S_LOG" | tail -40 || true
echo
echo "############ verdict ############"
BUILT=$(grep -c 'WP0 4.3: built wl_buffer' "$S_LOG" || true)
FAILED=$(grep -cE 'WP0 4\.3: (step [123]/3|no |resource [0-9]+ has no)' "$S_LOG" || true)
PROTO=$(grep -cE 'protocol error|send_request .* failed|panicked' "$S_LOG" || true)
echo "wl_buffers built from tokens : $BUILT"
echo "token refusals               : $FAILED"
echo "protocol errors / panics     : $PROTO"
if [ "$BUILT" -gt 0 ] && [ "$PROTO" -eq 0 ]; then
  echo "PASS (4.3): S built a real wl_buffer from a relayed token, with no protocol error."
  echo "            Whether the cube APPEARED is 4.5 and is a separate observation."
else
  echo "NOT YET (4.3): see $S_LOG on S and /tmp/rayland-c-wp0.log, /tmp/vkcube.log on $C_HOST."
  exit 1
fi
