#!/usr/bin/env bash
#
# THE DEMO: an unmodified vkcube running on the riscv64 milkv board, drawn by dop561's GPU, in its
# own window on dop561's REAL desktop.
#
# Not a measurement — there is no headless compositor here and nothing is scored. This exists so a
# human can look at the screen. Every sweep in `wp0-milkv-ab.sh` runs against headless weston, which
# is right for figures and shows nothing; this points S at the live session instead.
#
# The application and `rayland-c` run inside the board's Debian sid chroot (`/mnt/build/sid`); the
# host OS cannot run a modern Vulkan stack at all. See `/mnt/build/README`.
#
# Cleanup is by exact PID only, captured at launch.
#
#   SECONDS_TO_RUN=120 scripts/milkv-demo.sh
set -uo pipefail

SECONDS_TO_RUN="${SECONDS_TO_RUN:-120}"
PORT="${PORT:-9412}"
C_HOST="${C_HOST:-milkv.localdomain}"
C_BIN="${C_BIN:-/tmp/rv-c-HIER}"
S_BIN="${S_BIN:-/tmp/s-NEW}"
APP="${APP:-/usr/bin/vkcube}"
# **`--gpu_number` is an INDEX into the list Venus exposes, not a name, and the index moves.** The
# project rule says "vkcube must run with --gpu_number 0"; its *intent* is "do not land on the NVIDIA
# RTX A500", which loses the device on 7 of 14 runs. On 2026-09-01 index 0 was the NVIDIA card on both
# the dionysus and milkv paths, and the failure is SILENT from outside: swapchain buffers are built,
# one commit happens, and then nothing is ever presented, with no error in any log. Default to the
# Intel device and verify below rather than trusting either number.
# `${VAR-default}`, NOT `${VAR:-default}`: an explicitly EMPTY `APP_ARGS` must stay empty. `vkgears`
# takes no `--gpu_number`, and the colon form silently substituted the default into it, so the app
# printed its usage and exited while the harness reported "0 frames" as though the relay had failed.
APP_ARGS="${APP_ARGS---gpu_number 1}"
# The owner's live session. Not headless weston — the whole point is that it is visible.
DISPLAY_SOCKET="${DISPLAY_SOCKET:-wayland-1}"
SSH_OPTS=(-o BatchMode=yes -o IdentitiesOnly=no)
on_c() { ssh "${SSH_OPTS[@]}" "$C_HOST" "$@"; }
CHROOT=/mnt/build/sid
SOCK=/tmp/rl-demo.sock
WLNAME=wayland-rldemo
WLPATH=/run/user/0/$WLNAME
LOG="${LOG:-/tmp/milkv-demo}"
mkdir -p "$LOG"

on_c 'mountpoint -q /mnt/build/sid/proc' || {
  echo "chroot mounts are down; run: ssh $C_HOST 'sudo /mnt/build/chroot-mounts.sh up'" >&2; exit 1; }
leftovers=$(on_c "ps -o pid=,cmd= -C rayland-c -C vkcube 2>/dev/null")
[ -n "$leftovers" ] && { echo "REFUSING: leftovers on $C_HOST:"; echo "$leftovers"; exit 1; }

C_IP="$(getent ahostsv4 "$C_HOST" | awk '{print $1; exit}')"
S_IP="${S_IP:-$(ip -4 route get "$C_IP" | grep -oP 'src \K[\d.]+')}"

S_PID=""; CPID=""
cleanup() {
  [ -n "$CPID" ] && on_c "sudo kill $CPID 2>/dev/null" >/dev/null 2>&1
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null
}
# EXIT alone is not enough: bash does not run an EXIT trap when killed by an untrapped SIGTERM, so a
# demo stopped from outside left `rayland-s` holding the port and the NEXT run died with
# "Address already in use" — which looks like a relay failure and is not one.
trap cleanup EXIT INT TERM

echo "### S = $S_IP:$PORT, presenting into the LIVE session ($DISPLAY_SOCKET)"
# `env`, not a bare assignment prefix: bash decides which words are assignments **before** expanding
# them, so `${S_EVENT_LOG:+RAYLAND_S_EVENT_LOG=1}` becomes the *command name* when the variable is set
# and vanishes into a syntax error when it is not. `wp0-soak.sh` carries the same note for the same
# reason; this script had to learn it separately.
env WAYLAND_DISPLAY="$DISPLAY_SOCKET" XDG_RUNTIME_DIR=/run/user/$(id -u) \
  RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" \
  ${S_EVENT_LOG:+RAYLAND_S_EVENT_LOG=1} \
  "$S_BIN" > "$LOG/s.log" 2>&1 &
S_PID=$!
sleep 3
kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s did not stay up:"; tail -20 "$LOG/s.log"; exit 1; }

scp -q "${SSH_OPTS[@]}" "$C_BIN" "$C_HOST:/tmp/rayland-c-demo"
on_c "sudo cp /tmp/rayland-c-demo $CHROOT/opt/rayland/ && sudo chmod +x $CHROOT/opt/rayland/rayland-c-demo
      sudo rm -f $CHROOT$SOCK $CHROOT$WLPATH $CHROOT/tmp/rl-demo-c.log $CHROOT/tmp/rl-demo-app.log
      sudo chroot $CHROOT /bin/bash -c '
        export XDG_RUNTIME_DIR=/run/user/0
        mkdir -p \$XDG_RUNTIME_DIR && chmod 700 \$XDG_RUNTIME_DIR
        RAYLAND_WP_LOG=1 RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK \
        RAYLAND_C1_WAYLAND_DISPLAY=$WLPATH \
        nohup /opt/rayland/rayland-c-demo > /tmp/rl-demo-c.log 2>&1 &
        echo \$! > /tmp/rl-demo-c.pid
      '" >/dev/null 2>&1
sleep 4
CPID="$(on_c "sudo cat $CHROOT/tmp/rl-demo-c.pid 2>/dev/null" | tr -d ' \r\n')"
echo "### rayland-c on the board: pid $CPID"

echo "### launching $APP on the board for ${SECONDS_TO_RUN}s — LOOK AT YOUR SCREEN"
on_c "sudo chroot $CHROOT /bin/bash -c '
        export XDG_RUNTIME_DIR=/run/user/0
        VN_DEBUG=vtest \
        VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
        VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
        VTEST_SOCKET_NAME=$SOCK WAYLAND_DISPLAY=$WLNAME \
        env -u VK_LOADER_DRIVERS_SELECT timeout $SECONDS_TO_RUN $APP $APP_ARGS > /tmp/rl-demo-app.log 2>&1
      '" >/dev/null 2>&1 &
APP_JOB=$!
# **Check which device the application actually chose, and abort early if it is the wrong one.**
# Device loss on the NVIDIA card is silent — the run looks alive and simply never presents — so a demo
# that does not check this wastes the viewer's whole session staring at nothing.
sleep 6
gpu="$(on_c "sudo grep -m1 'Selected GPU' $CHROOT/tmp/rl-demo-app.log 2>/dev/null" || true)"
echo "### ${gpu:-<no GPU line yet>}"
case "$gpu" in
  *NVIDIA*) echo "### ABORTING: landed on the NVIDIA card, which loses the device and will never present." >&2
            echo "### Re-run with a different APP_ARGS index (the list order is not stable)." >&2
            exit 1 ;;
esac

for i in $(seq 1 "$SECONDS_TO_RUN"); do
  sleep 1
  if [ $((i % 10)) -eq 0 ]; then
    n=$(on_c "sudo grep -c 'forward obj 3 opcode 1 ' $CHROOT/tmp/rl-demo-c.log 2>/dev/null" | tr -d ' \r\n')
    echo "  t=${i}s  frames presented on dop561: ${n:-0}"
  fi
done
wait $APP_JOB 2>/dev/null
on_c "sudo cat $CHROOT/tmp/rl-demo-app.log" > "$LOG/app.log" 2>/dev/null
echo "### done. app output:"; tail -3 "$LOG/app.log"
