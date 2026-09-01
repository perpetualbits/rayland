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
APP_ARGS="${APP_ARGS:---gpu_number 0}"
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
trap cleanup EXIT

echo "### S = $S_IP:$PORT, presenting into the LIVE session ($DISPLAY_SOCKET)"
WAYLAND_DISPLAY="$DISPLAY_SOCKET" XDG_RUNTIME_DIR=/run/user/$(id -u) \
  RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" \
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
