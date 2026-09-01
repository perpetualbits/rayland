#!/usr/bin/env bash
#
# (c)2 — the readback-completion gate, proven over a real network.
# ============================================================================================
#
# WHAT THIS PROVES
#   `rayland-icosa-cpu` (the default; `APP=gpu` selects its shader-side twin) — 120 frames, a
#   spinning icosahedron textured with a per-frame CPU fractal
#   written into mapped HOST_COHERENT memory) runs on C (apollo) through `rayland-c`, is replayed
#   on S (dop561) by `rayland-s`, and read back. Before the readback-completion gate, ~2/120 frames
#   came back as the WHOLE PREVIOUS frame over a real link (0/120 on loopback) — a readback-delivery
#   lag on S, not a forward relay race (docs/design/2026-07-19-c2-true-remote-mapped-sync.md). After
#   the gate, every frame must match native-on-S across many runs.
#
# CORRECTNESS ASSERTION
#   Compare each relayed frame against the same fixture run NATIVELY ON S (same Intel GPU), so
#   only the transport differs and every frame must be bit-identical. Do NOT compare against the app
#   run on C (AMD GPU, a different rasteriser).
#
# WHY no VN_DEBUG=no_abort (do not add it): Mesa's ~3.5s stall-abort is the stall detector.
#
# EXPECTED RESULT TODAY (read before treating a non-zero exit as a regression): the readback-
#   completion gate is a *measured partial fix*. It takes the rate from most-runs-losing-1-4-frames
#   to ~10/11 runs fully clean, but a ~1/11 residual of the same N==N-1 signature REMAINS — the
#   design's §9 C-side release race (the RingProgress head-advance releases the app before the gated
#   readback delivery lands on C). So an occasional `FAIL` here is the KNOWN §9 residual, not a new
#   break; the fix for it is a separate return-path-ordering change. See
#   docs/design/2026-07-19-c2-readback-completion-gate.md §9.
#
# Usage:  scripts/c2-icosa-two-machine.sh [RUNS]     # default 10 runs; exits non-zero on any stale frame
set -euo pipefail

C_HOST="${C_HOST:-apollo}"
S_IP="${S_IP:-192.168.1.192}"
PORT="${PORT:-9402}"
RUNS="${1:-10}"
# Which fixture. The two exist as a PAIR — same geometry, same schedule, same render loop, differing
# only in whether the per-frame fractal is computed on C's CPU into mapped memory (~1 MiB/frame with
# no interceptable call) or in a fragment shader (~80 bytes of uniforms). Running only one of them
# measures a number; running both measures how cost scales with mapped-write volume, which is the
# question they were built to answer.
APP="${APP:-cpu}"
FIXTURE="rayland-icosa-$APP"
# Which of Venus's feedback mechanisms Mesa may use. Each one it does NOT have replaces a shared
# status page with a synchronous round trip, so turning them on is the obvious lever on frame time —
# and it was tried, on 2026-07-26, and it does not hold. Both halves are worth knowing:
#
#   * `no_fence_feedback` is LOAD-BEARING and must stay. (c)2's completion barrier
#     (`Applier::reply_arena_fence_signaled`) works by spotting the application's `vkGetFenceStatus`
#     reply reading VK_SUCCESS, and fence feedback removes that poll entirely. Measured: enabling it
#     gives exit 134 and 0 frames, immediately and every time.
#
#   * Semaphore, event and query feedback are worth 1.23x and their status is UNRESOLVED — this
#     comment used to say they "look safe and ARE NOT", and that is stronger than the evidence.
#     Measured 1.23x on icosa-gpu over loopback (median draw_readback 48.7 ms -> 39.5 ms), all 120
#     frames bit-identical, and then one run of ten lost to a silent Venus SIGABRT in this sweep.
#     That single event was hunted through 82 further clean runs (60 of them unattended with core
#     capture genuinely armed; no core was ever produced). 1 failure in 92 against 0 in 20 is NOT a
#     significant difference, so the failure cannot be pinned on feedback at all.
#
#     The explanation this comment used to give is REFUTED, and must not be repeated: "(c)1 does not
#     relay the feedback pages" is false. `emit_blob_writes` excludes only rings, and
#     `take_bytes_s_wrote` detects change by diffing a shadow, so it catches writes virglrenderer's
#     GPU makes directly rather than only relayed copies. Measured with all three feedbacks on: S
#     ships back res=2 and res=5 and nothing else, traffic within 0.1% of the feedback-off run.
#     There is no un-relayed feedback page in this workload.
#
#     A loopback pass still proves nothing here. The flags stay off because an unexplained
#     total-session loss is unexplained either way — not because feedback is known to break anything.
#
# `no_fence_feedback` is load-bearing in every arm; the other three are what the queued soak
# (`scripts/soak-failure-rate.sh`) exists to settle.
VN_PERF_SETTING="${VN_PERF_SETTING:-no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback}"
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
SOCK="/tmp/rl-c2-icosa.sock"

echo "### building rayland-c, rayland-s, $FIXTURE (release; the app must be fast) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -p rayland-c -p rayland-s -p $FIXTURE

# *** PIN WHAT S's VULKAN ENUMERATES, ON BOTH SIDES OF THE COMPARISON ***
# The fixtures ask `ash` for a physical device and take one; they have no `--gpu_number`. dop561 has
# an Intel iGPU and an NVIDIA RTX A500 whose VK_ERROR_DEVICE_LOST is SILENT. Two ways that corrupts
# this test: the remoted run losing the device and producing nothing, or the native baseline landing
# on a DIFFERENT RASTERISER than the remoted run, which makes every frame differ for a reason that
# has nothing to do with the relay. `S_ICD=` (empty) restores full enumeration.
S_ICD="${S_ICD-/usr/share/vulkan/icd.d/intel_icd.json}"
echo "### native baseline on S (Intel GPU, no Venus) ###"
rm -rf /tmp/icosa-native && mkdir -p /tmp/icosa-native
env ${S_ICD:+VK_ICD_FILENAMES=$S_ICD} "$BIN/$FIXTURE" /tmp/icosa-native >/tmp/icosa-native.csv
echo "native frames: $(ls /tmp/icosa-native/frame_*.png | wc -l)"

echo "### deploy C-side binaries to $C_HOST ###"
scp -q "$BIN/rayland-c" "$BIN/$FIXTURE" "$C_HOST:/tmp/"
ssh "$C_HOST" "chmod +x /tmp/rayland-c /tmp/$FIXTURE"

S_PID=""
# Kill only by exact PID — the local rayland-s by the PID we captured, and the remote C-side by the
# PIDs it wrote to /tmp on launch. Never `pkill`/pattern: a pattern kill can match unrelated processes.
cleanup() {
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  ssh "$C_HOST" 'for p in /tmp/rayland-c.pid /tmp/rayland-app.pid; do kill "$(cat "$p" 2>/dev/null)" 2>/dev/null || true; done' 2>/dev/null || true
}
trap cleanup EXIT

total_stale=0
for run in $(seq 1 "$RUNS"); do
  ssh "$C_HOST" 'rm -rf /tmp/icosa-relay; mkdir -p /tmp/icosa-relay'
  env ${S_ICD:+VK_ICD_FILENAMES=$S_ICD} RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"/tmp/rayland-s-c2-$run.log" 2>&1 &
  S_PID=$!; sleep 3
  kill -0 "$S_PID" 2>/dev/null || { echo "rayland-s died:"; cat "/tmp/rayland-s-c2-$run.log"; exit 1; }
  ssh "$C_HOST" "
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-icosa.log 2>&1 &
    echo \$! > /tmp/rayland-c.pid
    sleep 3
    VN_DEBUG=vtest VN_PERF="$VN_PERF_SETTING" \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT /tmp/$FIXTURE /tmp/icosa-relay >/tmp/icosa-relay.csv 2>&1 &
    app_pid=\$!; echo \$app_pid > /tmp/rayland-app.pid
    wait \$app_pid || echo APP_EXIT_NONZERO
    # Retire THIS run's daemon. Nothing used to: only the exit trap killed a rayland-c, and only the
    # single PID left in the file, so every earlier run's daemon survived to the end of the sweep --
    # all of them bound to the same vtest socket as the next run's application. That is a live
    # confound for any per-run result this sweep produces, and this is the sweep whose 9-of-10 lost
    # one session to an unexplained Venus SIGABRT. NOT established as that failure's cause; it is a
    # mechanism that could produce exactly that shape and that was present the whole time.
    # Kill by the exact PID this run recorded, and only after confirming it is still our binary.
    cpid=\$(cat /tmp/rayland-c.pid 2>/dev/null)
    case \"\$(readlink /proc/\$cpid/exe 2>/dev/null)\" in
      /tmp/rayland-c|'/tmp/rayland-c (deleted)') kill \"\$cpid\" 2>/dev/null ;;
    esac
    rm -f /tmp/rayland-c.pid /tmp/rayland-app.pid
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
