#!/usr/bin/env bash
#
# The icosahedron fixtures with the riscv64 milkv board as machine C.
# =============================================================================================
#
# WHY THIS EXISTS
#   `rayland-icosa-cpu` and `rayland-icosa-gpu` are **our** fixtures: instrumented, modifiable, and
#   built specifically to isolate how cost scales with the volume of *uninterceptable mapped writes*
#   (the (c)2 problem). Every measurement taken of them so far has used an x86_64 machine as C, where
#   C has CPU to spare — which is the case Rayland's premise says is NOT the interesting one. The
#   board is the weak C the whole design is aimed at, and until now the fixtures had never been run
#   against it. `vkcube` and `vkgears` have; they are somebody else's programs and cannot be
#   instrumented or changed.
#
#   This is `c2-icosa-two-machine.sh` with the board's topology, not a new experiment. Keep the two
#   in step where they overlap.
#
# WHY IT IS A SEPARATE SCRIPT, LIKE `wp0-milkv-ab.sh`
#   Everything on the board that touches Vulkan must run inside the **Debian sid chroot**
#   (`/mnt/build/sid`, Mesa 26.1.6, `virtio_icd.json`). The host root is a Debian ports snapshot
#   frozen at 2022-12-25 whose glibc is 2.36 — a binary cross-built against sid's 2.43 dies there
#   with `GLIBC_2.39 not found` before `main`, which is how this script learned the rule. Nothing is
#   ever built on the board: both the fixture and `rayland-c` are cross-compiled on the laptop.
#
# WHAT IS COMPARED, AND WHY THAT IS A VALID TEST
#   Each relayed frame is compared against the SAME fixture run **natively on S**, on S's own Intel
#   GPU with no Venus in the path, so the only difference between the two is the transport and every
#   frame must be bit-identical.
#
#   That comparison spans two architectures — S computes the fixture's per-frame fractal on x86_64,
#   the board computes it on riscv64 — and it is only meaningful because `rayland-icosa-core` builds
#   its `log2`/`sin`/`cos` out of IEEE-754 basic operations precisely so they evaluate bit-identically
#   on any host. **That contract had only ever been executed on x86_64.** It was checked on the board
#   before this script was trusted: the crate's 29 tests, including the committed golden bit-pattern
#   tables in `tests/log2_table.rs` and `tests/sin_cos_table.rs`, all pass inside the chroot on
#   riscv64. If they ever stop passing there, every result from this script becomes a statement about
#   arithmetic rather than about Rayland, so re-run them before believing a diff:
#
#     cargo test --release --no-run -p rayland-icosa-core --target riscv64gc-unknown-linux-gnu
#     # then scp the three test binaries into /mnt/build/sid/tmp and run them under chroot
#
# *** BOTH SIDES ARE PINNED TO S's INTEL GPU, AND THIS IS NOT OPTIONAL ***
#   The fixtures ask `ash` for a physical device and take one; they have no `--gpu_number`. dop561
#   has an Intel iGPU and an NVIDIA RTX A500 whose `VK_ERROR_DEVICE_LOST` is **silent** — buffers are
#   created, nothing is ever presented, no log says why (`CLAUDE.md`, 2026-07-26). Two distinct ways
#   that would corrupt this test: the remoted run losing the device and producing nothing, or the
#   native baseline landing on a *different rasteriser* than the remoted run, which would make every
#   frame differ for a reason that has nothing to do with the relay. So the pin is applied to the
#   native baseline **and** to `rayland-s`. `S_ICD=` (empty) restores full enumeration.
#
# CLEANUP
#   Only ever by exact PID, captured at launch — the board's side via PID files written inside the
#   chroot. This script does not pattern-kill.
#
# Usage:
#   scripts/c2-icosa-milkv.sh                 # 1 run of the CPU fixture
#   APP=gpu RUNS=2 scripts/c2-icosa-milkv.sh  # the GPU fixture (80 bytes/frame instead of ~1 MiB)
set -uo pipefail

APP="${APP:-cpu}"                       # cpu | gpu — which fixture
RUNS="${RUNS:-1}"
PORT="${PORT:-9413}"
C_HOST="${C_HOST:-milkv.localdomain}"
CHROOT=/mnt/build/sid
SOCK=/tmp/rl-icosa-milkv.sock
OUT="${OUT:-/tmp/icosa-milkv-$(date +%Y%m%d-%H%M%S)}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
RV_BIN="${RV_BIN:-/tmp/rv/riscv64gc-unknown-linux-gnu/release}"
FIXTURE="rayland-icosa-$APP"
# Venus feedback settings, carried verbatim from `c2-icosa-two-machine.sh`. `no_fence_feedback` is
# LOAD-BEARING: (c)2's completion barrier works by spotting the application's `vkGetFenceStatus`
# reply reading VK_SUCCESS, and fence feedback removes that poll entirely (measured: exit 134, zero
# frames, every time). Do not "tidy" this list.
# `RELAXSTAT=1` arms C's stage recorder (`RAYLAND_C1_RELAXSTAT`), which brackets each ring delta
# into RingShipped -> SyncPrepared (the forward path's WORK: every blob diffed against its baseline
# and the batch serialized) -> SyncSent (written and flushed). That first interval is where the blob
# diff lives, and it is the interval any dirty-page-tracking work would be trying to remove — so it
# is what says whether that work is worth doing on THIS machine. The recorder does one clock read
# and one array store per event and is designed not to perturb; see `relaxstat.rs`.
VN_PERF_SETTING="${VN_PERF_SETTING:-no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback}"
# `IdentitiesOnly yes` is set globally in the owner's ssh config with no `Host` entry for the board,
# so a fresh connection offers no key and is refused once a ControlPersist master expires.
SSH_OPTS=(-o BatchMode=yes -o IdentitiesOnly=no)
on_c() { ssh "${SSH_OPTS[@]}" "$C_HOST" "$@"; }

mkdir -p "$OUT"
case "$APP" in cpu|gpu) ;; *) echo "APP must be cpu or gpu, got '$APP'"; exit 1 ;; esac

# ---- Pre-flight ------------------------------------------------------------------------------
on_c "mountpoint -q $CHROOT/proc" || {
  echo "chroot mounts are down; run: ssh $C_HOST 'sudo /mnt/build/chroot-mounts.sh up'" >&2; exit 1; }
leftovers=$(on_c "ps -o pid=,cmd= -C rayland-c -C $FIXTURE 2>/dev/null")
[ -n "$leftovers" ] && { echo "REFUSING: leftovers on $C_HOST:"; echo "$leftovers"; exit 1; }
[ -x "$RV_BIN/$FIXTURE" ] || {
  echo "no riscv64 $FIXTURE at $RV_BIN/. Cross-build it:"
  echo "  CARGO_TARGET_RISCV64GC_UNKNOWN_LINUX_GNU_LINKER=riscv64-linux-gnu-gcc \\"
  echo "  CC_riscv64gc_unknown_linux_gnu=riscv64-linux-gnu-gcc \\"
  echo "  CARGO_TARGET_DIR=/tmp/rv cargo build --release -p $FIXTURE --target riscv64gc-unknown-linux-gnu"
  exit 1; }
[ -x "$RV_BIN/rayland-c" ] || { echo "no riscv64 rayland-c at $RV_BIN/rayland-c"; exit 1; }

S_ICD="${S_ICD-/usr/share/vulkan/icd.d/intel_icd.json}"

echo "### building S-side binaries (release) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -q -p rayland-s -p "$FIXTURE" || exit 1

# ---- The native baseline, on S's own GPU with no Venus in the path ---------------------------
echo "### native baseline on S ($FIXTURE, Intel, no Venus) ###"
rm -rf "$OUT/native" && mkdir -p "$OUT/native"
env ${S_ICD:+VK_ICD_FILENAMES=$S_ICD} "$BIN/$FIXTURE" "$OUT/native" > "$OUT/native.csv" 2>&1 || {
  echo "native baseline failed:"; tail -5 "$OUT/native.csv"; exit 1; }
native_frames=$(ls "$OUT/native"/frame_*.png 2>/dev/null | wc -l)
echo "native frames: $native_frames"
[ "$native_frames" -gt 0 ] || { echo "native baseline produced no frames"; exit 1; }

# ---- Deploy into the chroot -------------------------------------------------------------------
echo "### deploying riscv64 binaries into $CHROOT ###"
scp -q "${SSH_OPTS[@]}" "$RV_BIN/rayland-c" "$RV_BIN/$FIXTURE" "$C_HOST:/tmp/" || exit 1
on_c "sudo mkdir -p $CHROOT/opt/rayland
      sudo cp /tmp/rayland-c /tmp/$FIXTURE $CHROOT/opt/rayland/
      sudo chmod +x $CHROOT/opt/rayland/rayland-c $CHROOT/opt/rayland/$FIXTURE" || exit 1

S_IP="${S_IP:-$(ip -4 route get "$(getent ahostsv4 "$C_HOST" | awk '{print $1; exit}')" | grep -oP 'src \K[\d.]+')}"

S_PID=""
cleanup() {
  on_c "sudo chroot $CHROOT /bin/bash -c '
          for p in /tmp/rayland-c.pid /tmp/rayland-app.pid; do
            [ -f \"\$p\" ] && kill \"\$(cat \"\$p\")\" 2>/dev/null
          done'" >/dev/null 2>&1
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null
}
# EXIT alone is not enough: bash does not run an EXIT trap when killed by an untrapped SIGTERM, so a
# run stopped from outside would leave `rayland-s` holding the port and the next run would die with
# "Address already in use" — which looks like a relay failure and is not one.
trap cleanup EXIT INT TERM

total_stale=0
for run in $(seq 1 "$RUNS"); do
  RD="$OUT/run$run"; mkdir -p "$RD"
  on_c "sudo rm -rf $CHROOT/tmp/icosa-relay && sudo mkdir -p $CHROOT/tmp/icosa-relay"

  env ${S_ICD:+VK_ICD_FILENAMES=$S_ICD} \
    RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" \
    "$BIN/rayland-s" > "$RD/s.log" 2>&1 &
  S_PID=$!
  sleep 3
  kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s did not stay up:"; tail -10 "$RD/s.log"; exit 1; }

  echo "### run $run/$RUNS: $FIXTURE on the board, replayed on S ###"
  start=$(date +%s)
  on_c "sudo chroot $CHROOT /bin/bash -c '
    export XDG_RUNTIME_DIR=/run/user/0
    mkdir -p \$XDG_RUNTIME_DIR && chmod 700 \$XDG_RUNTIME_DIR
    rm -f $SOCK
    RAYLAND_C1_METRICS=1 ${RELAXSTAT:+RAYLAND_C1_RELAXSTAT=1} ${BLOBSCAN:+RAYLAND_C1_BLOBSCAN=1} \
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK \
      nohup /opt/rayland/rayland-c > /tmp/rl-icosa-c.log 2>&1 &
    echo \$! > /tmp/rayland-c.pid
    sleep 3
    VN_DEBUG=vtest VN_PERF=$VN_PERF_SETTING \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT /opt/rayland/$FIXTURE /tmp/icosa-relay > /tmp/rl-icosa-app.log 2>&1 &
    app_pid=\$!; echo \$app_pid > /tmp/rayland-app.pid
    wait \$app_pid || echo APP_EXIT_NONZERO
  '" 2>&1 | tail -3
  elapsed=$(( $(date +%s) - start ))

  # Collect: the fixture's frames, its own CSV, and C's metrics line.
  on_c "sudo tar -C $CHROOT/tmp -cf - icosa-relay" 2>/dev/null | tar -C "$RD" -xf - 2>/dev/null
  on_c "sudo cat $CHROOT/tmp/rl-icosa-app.log" > "$RD/app.log" 2>/dev/null
  on_c "sudo cat $CHROOT/tmp/rl-icosa-c.log"   > "$RD/c.log"   2>/dev/null
  kill "$S_PID" 2>/dev/null; S_PID=""
  sleep 1

  relayed=$(ls "$RD/icosa-relay"/frame_*.png 2>/dev/null | wc -l)
  stale=0; missing=0
  for f in "$OUT/native"/frame_*.png; do
    b=$(basename "$f")
    if [ ! -f "$RD/icosa-relay/$b" ]; then missing=$((missing + 1))
    elif ! cmp -s "$f" "$RD/icosa-relay/$b"; then stale=$((stale + 1)); fi
  done
  echo "run $run/$RUNS: ${elapsed}s  frames=$relayed/$native_frames  differing=$stale  missing=$missing"
  total_stale=$((total_stale + stale + missing))
done

echo
echo "############ $FIXTURE, milkv as C ############"
echo "runs=$RUNS  total differing-or-missing frames: $total_stale"
echo "output: $OUT"
[ "$total_stale" -eq 0 ] && echo "PASS: every relayed frame is bit-identical to native-on-S"
