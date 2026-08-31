#!/usr/bin/env bash
#
# WP0 A/B with the riscv64 milkv board as machine C.
# =============================================================================================
#
# WHY THIS EXISTS SEPARATELY FROM `wp0-soak.sh`
#   `wp0-soak.sh` runs the application and `rayland-c` directly on C over ssh. On milkv that is
#   impossible: the host OS is a Debian *ports* snapshot frozen at 2022-12-25 (glibc 2.36, Mesa
#   22.3.5) whose only Vulkan ICD is `radeon_icd.riscv64.json`, and whose apt reaches nothing newer.
#   The working stack lives in a **Debian sid chroot on a second card** (`/mnt/build/sid`, Mesa 26.1.6,
#   `virtio_icd.json`), built by another session; `/mnt/build/README` documents it. So every command
#   that touches Vulkan has to run inside that chroot, which is a different enough topology to deserve
#   its own script rather than a fourth branch in the soak's.
#
#   The host root has ~1.1 GB free against a 9 GB debug target directory, so **nothing is ever built
#   on the board**. Both arms are cross-compiled on the laptop
#   (`--target riscv64gc-unknown-linux-gnu`, release, ~2 minutes) and copied in. That also guarantees
#   one toolchain built both arms, which matters more for a comparison than either arm's absolute
#   speed.
#
# WHAT IT MEASURES, AND WHY THIS BOARD
#   The 2026-09-01 chunk-size fix removed ~10 ms of CPU per ring delta **on C**, and C is the machine
#   Rayland's whole premise says may be weak. On the laptop that fix was worth 1.78x. On a 4-core
#   riscv64 board at ~5 fps it should be worth more, and if it is not, the model of where the time
#   goes is wrong. That is the question; everything below is scaffolding for it.
#
#   Primary metric is the **median inter-frame gap**, computed exactly as `wp0-soak.sh` computes it:
#   from the `t_ns=` stamps on C's own `forward obj 3 opcode 1` proxy log lines (a `wl_surface.attach`
#   forwarded), so no sampler runs and nothing is polled. `attaches` is reported but is NOT a rate.
#
# CLEANUP
#   Only ever by exact PID, captured at launch. This script does not pattern-kill.
#
# USAGE
#   scripts/wp0-milkv-ab.sh                 # 6 interleaved pairs, 25 s each
#   PAIRS=10 SECS=30 scripts/wp0-milkv-ab.sh
#   A_BIN=/tmp/x B_BIN=/tmp/y scripts/wp0-milkv-ab.sh
set -uo pipefail

PAIRS="${PAIRS:-6}"
SECS="${SECS:-25}"
PORT="${PORT:-9411}"
C_HOST="${C_HOST:-milkv.localdomain}"
# The two arms. Cross-built on the laptop; see the header.
A_BIN="${A_BIN:-/tmp/rv-c-BEFORE}"
B_BIN="${B_BIN:-/tmp/rv-c-AFTER}"
APP="${APP:-/usr/bin/vkcube}"
APP_ARGS="${APP_ARGS:---gpu_number 0}"
OUT="${OUT:-/tmp/milkv-ab-$(date +%Y%m%d-%H%M%S)}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/debug"
WESTON_SOCKET="${WESTON_SOCKET:-wl-milkv}"

# `IdentitiesOnly yes` is set globally in the owner's ssh config with no `Host milkv` entry, so a
# fresh connection offers no key and is refused; earlier sessions only worked by riding a
# `ControlPersist` master. Letting the agent offer its keys is the non-invasive fix — this script does
# not edit the owner's ssh configuration.
SSH_OPTS=(-o BatchMode=yes -o IdentitiesOnly=no)
on_c() { ssh "${SSH_OPTS[@]}" "$C_HOST" "$@"; }

CHROOT=/mnt/build/sid
SOCK=/tmp/rl-ab.sock
WLNAME=wayland-rlab
WLPATH=/run/user/0/$WLNAME

mkdir -p "$OUT"
echo "### output: $OUT"

# ---- Pre-flight -----------------------------------------------------------------------------
on_c 'mountpoint -q /mnt/build/sid/proc' || {
  echo "REFUSING TO RUN: the chroot bind mounts are down on $C_HOST." >&2
  echo "  Restore them with:  sudo /mnt/build/chroot-mounts.sh up" >&2
  exit 1
}
for f in "$A_BIN" "$B_BIN"; do
  [ -x "$f" ] || { echo "missing arm binary $f — cross-build it first (see the header)" >&2; exit 1; }
done
leftovers=$(on_c "ps -o pid=,cmd= -C rayland-c -C vkcube 2>/dev/null")
if [ -n "$leftovers" ]; then
  echo "REFUSING TO RUN: processes from an earlier sweep are still alive on $C_HOST:" >&2
  echo "$leftovers" >&2
  echo "End them by PID, then re-run. This script will not pattern-kill." >&2
  exit 1
fi

C_IP="$(getent ahostsv4 "$C_HOST" | awk '{print $1; exit}')"
S_IP="${S_IP:-$(ip -4 route get "$C_IP" | grep -oP 'src \K[\d.]+')}"
echo "### S=$S_IP:$PORT   C=$C_HOST ($C_IP)"

# ---- S's compositor, started once and shared by every run -----------------------------------
# Headless weston for the reasons `wp0-soak.sh` documents at length: it composites on a timer with no
# output, so a run is never scored on whether a desktop happened to be drawing. `--idle-time=0` is not
# optional — weston stops compositing after 300 s otherwise and every later run looks like a stall.
if ! pgrep -f "weston.*--socket=$WESTON_SOCKET" >/dev/null; then
  echo "### starting headless weston on S (socket $WESTON_SOCKET)"
  __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json \
    setsid weston --backend=headless --renderer=gl --width=1280 --height=1024 \
    --socket="$WESTON_SOCKET" --idle-time=0 --no-config > "$OUT/weston.log" 2>&1 &
  sleep 6
fi

printf 'pair\tarm\tattaches\tframe_gaps\tmedian_gap_ms\tstall_gaps\tlongest_ms\n' > "$OUT/runs.tsv"

run_one() {   # $1 = arm label, $2 = binary, $3 = pair number
  local arm="$1" bin="$2" pair="$3"
  local rd="$OUT/pair$pair-$arm"; mkdir -p "$rd"
  local S_PID="" CPID=""

  # S: the GPU machine. RAYLAND_C1_NO_PRESENT keeps S off its own screen; the app's window is the
  # thing under test and it goes to weston.
  env WAYLAND_DISPLAY="$WESTON_SOCKET" RAYLAND_C1_NO_PRESENT=1 \
    RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" > "$rd/s.log" 2>&1 &
  S_PID=$!
  sleep 3
  if ! kill -0 "$S_PID" 2>/dev/null; then
    echo "  $arm pair$pair: rayland-s did not stay up"; return
  fi

  # C: copy this arm in, then start the daemon INSIDE the chroot. RAYLAND_WP_LOG is what produces the
  # timestamped attach lines the frame gap is derived from — without it there is no timeline at all.
  scp -q "${SSH_OPTS[@]}" "$bin" "$C_HOST:/tmp/rayland-c-arm" 2>/dev/null
  on_c "sudo cp /tmp/rayland-c-arm $CHROOT/opt/rayland/rayland-c-arm && sudo chmod +x $CHROOT/opt/rayland/rayland-c-arm
        sudo rm -f $CHROOT$SOCK $CHROOT$WLPATH $CHROOT/tmp/rl-ab-c.log $CHROOT/tmp/rl-ab-app.log
        sudo chroot $CHROOT /bin/bash -c '
          export XDG_RUNTIME_DIR=/run/user/0
          mkdir -p \$XDG_RUNTIME_DIR && chmod 700 \$XDG_RUNTIME_DIR
          RAYLAND_WP_LOG=1 RAYLAND_C1_METRICS=1 \
          RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK RAYLAND_C1_WAYLAND_DISPLAY=$WLPATH \
          nohup /opt/rayland/rayland-c-arm > /tmp/rl-ab-c.log 2>&1 &
          echo \$! > /tmp/rl-ab-c.pid
        '" >/dev/null 2>&1
  sleep 4
  CPID="$(on_c "sudo cat $CHROOT/tmp/rl-ab-c.pid 2>/dev/null" | tr -d ' \r\n')"

  # The application, for a fixed wall-clock time. `timeout` returns 124 on the normal path.
  on_c "sudo chroot $CHROOT /bin/bash -c '
          export XDG_RUNTIME_DIR=/run/user/0
          VN_DEBUG=vtest \
          VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
          VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json \
          VTEST_SOCKET_NAME=$SOCK WAYLAND_DISPLAY=$WLNAME \
          env -u VK_LOADER_DRIVERS_SELECT timeout $SECS $APP $APP_ARGS > /tmp/rl-ab-app.log 2>&1
        '" >/dev/null 2>&1

  # Stop the daemon by the exact PID captured above, never by pattern.
  [ -n "$CPID" ] && on_c "sudo kill $CPID 2>/dev/null" >/dev/null 2>&1
  kill "$S_PID" 2>/dev/null
  sleep 1

  on_c "sudo cat $CHROOT/tmp/rl-ab-c.log 2>/dev/null"   > "$rd/c.log"
  on_c "sudo cat $CHROOT/tmp/rl-ab-app.log 2>/dev/null" > "$rd/app.log"

  # The timeline, and the same scoring `wp0-soak.sh` uses: median inter-attach gap, plus how many gaps
  # exceed 10x it (contamination, reported rather than averaged in).
  awk '/forward obj 3 opcode 1 / { if (match($0, /t_ns=[0-9]+/)) print substr($0, RSTART+5, RLENGTH-5) }' \
      "$rd/c.log" > "$rd/timeline.dat"
  local attaches; attaches=$(grep -c 'forward obj 3 opcode 1 ' "$rd/c.log" 2>/dev/null || echo 0)
  local empty longest samples medgap
  read -r empty longest samples medgap < <(awk '
    NR == FNR { if (FNR > 1) g[++k] = $1 - prev; prev = $1; next }
    FNR == 1 { asort(g); med = (k ? g[int(k/2)+1] : 0) }
    { }
    END {
      for (i = 1; i <= k; i++) if (med > 0 && g[i] > 10 * med) { e++; if (g[i] > worst) worst = g[i] }
      printf "%d %d %d %d\n", e + 0, worst / 1000000, k + 0, med / 1000000
    }' "$rd/timeline.dat" "$rd/timeline.dat")
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$pair" "$arm" "$attaches" "$samples" "$medgap" "$empty" "$longest" >> "$OUT/runs.tsv"
  printf '  pair%-2s %-6s attaches=%-5s median_gap=%-6s ms  stalls=%s\n' "$pair" "$arm" "$attaches" "$medgap" "$empty"
}

for p in $(seq 1 "$PAIRS"); do
  run_one BEFORE "$A_BIN" "$p"
  run_one AFTER  "$B_BIN" "$p"
done

echo
echo "############ milkv A/B ############"
awk -F'\t' 'NR>1 && $5>0 { v[$2] = v[$2] " " $5 } END { for (a in v) print a ":" v[a] }' "$OUT/runs.tsv"
echo "per-run table: $OUT/runs.tsv"
