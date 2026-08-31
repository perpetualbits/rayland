#!/usr/bin/env bash
#
# WP0 — a FAILURE RATE for the end-to-end path, and per-frame TRAFFIC with spread.
# =============================================================================================
#
# WHY THIS EXISTS
#   WP0 reached end-to-end on 2026-08-29 and the report of that day rested on roughly a dozen
#   ad-hoc runs and a single before/after traffic pair. Neither is a number. This harness turns
#   both claims into measurements that survive repetition, applying the hazard that session wrote
#   down: *a claim in a comment is not a measurement.*
#
# ------------------------------------------------------------------------------------------
# WHY A HEADLESS WESTON, AND NOT THE DESKTOP COMPOSITOR
#   A Wayland compositor emits `wl_surface.frame` callbacks only for surfaces it actually
#   COMPOSITES, and releases a buffer only when it stops using one. An application that presents
#   correctly will therefore sit idle, by design, whenever its window is not being drawn — which
#   is exactly what happens on a desktop that blanks, locks, switches workspace, or simply has
#   the window behind something. An overnight soak on COSMIC would score every one of those as a
#   liveness failure, and would be measuring the screensaver.
#
#   Nesting a compositor inside COSMIC does not help: a nested compositor that stops receiving
#   frame callbacks from its host throttles its own repaint and withholds callbacks from its own
#   clients in turn — the same failure with an extra layer.
#
#   weston's headless backend composites on a timer with no output at all, which is the property
#   actually needed. `cosmic-comp` has no headless backend (only winit and udev), so weston it is.
#
#   TWO THINGS THAT MUST BE TRUE, both verified before this script was trusted (2026-08-29):
#     1. Headless weston must import a dma-buf, which needs the GL renderer, not pixman.
#        `--renderer=gl` loads `gl-renderer.so` and reports "dmabuf support: modifiers".
#     2. It must composite on the SAME GPU the frames are rendered on. Left alone, weston's EGL
#        picks the NVIDIA card while `rayland-s` renders on Intel `/dev/dri/renderD128`, making
#        every import a cross-GPU one. `__EGL_VENDOR_LIBRARY_FILENAMES` pinned to Mesa puts
#        weston on "Mesa Intel(R) Iris(R) Xe Graphics", matching the renderer.
#     3. `--idle-time=0`, and this one is not optional. Weston idles out after 300 s by default and
#        STOPS COMPOSITING, at which point it withholds frame callbacks exactly as a blanking desktop
#        does. Without it, a sweep is healthy for its first few minutes and then every later run scores
#        a liveness failure — which is what happened here, and looked precisely like the application
#        stalling: the first runs did 400+ attaches, later ones exactly 1, and S's log showed the frame
#        callback simply never arriving. The compositor is a participant in this measurement, and this
#        is the second time in two days that a compositor declining to draw has been mistaken for a
#        Rayland defect.
#   The verifying spike: 801 attaches in ~40 s (~20 fps), 1596 frame callbacks, zero errors.
#
# ------------------------------------------------------------------------------------------
# WHAT COUNTS AS A FAILURE (decided from the logs, because a soak has no human watching a cube)
#   A run FAILS if any of these holds. Each is reported as its own mode, because a rate that
#   mixes a protocol error with a liveness miss is two rates wearing one number.
#
#     invalid_object  - any `Invalid ObjectId` on either daemon. This is the class of bug that
#                       silently unmapped the window on 2026-08-29: a cached handle to a
#                       destroyed object. It never appears in a healthy run.
#     event_drop      - any event drop EXCEPT the one known and accepted exclusion,
#                       `carries-fd` on `wl_keyboard.keymap`, which cannot cross a network and
#                       which no application here blocks on. Excluding it is the one place this
#                       definition deliberately stops measuring something; see the diary.
#     protocol        - a Wayland protocol error, a `catch_unwind` trip, or a panic on S.
#     early_exit      - a daemon or the application exiting before the harness stops it.
#     liveness        - fewer than $MIN_ATTACHES surface attaches in $APP_SECONDS seconds.
#
#   THE LIVENESS FLOOR IS DELIBERATELY GENEROUS. The measured healthy rate against headless
#   weston is ~20 fps. The floor below is 2 fps — a TENTH of healthy — because the failure this
#   is meant to catch is "the app stopped", not "the app was slow", and a tight floor would turn
#   ordinary scheduling variance into a fake failure rate. A run that manages 3 fps is not
#   healthy, but it is not the stall this project keeps shipping, and it should not be counted as
#   one without someone looking at it.
#
# ------------------------------------------------------------------------------------------
# TRAFFIC MODE (`MODE=traffic`)
#   Runs a FIXED FRAME COUNT rather than a fixed duration, via `vkcube --c N`. The 2026-08-29
#   before/after pair compared 120 frames against 96 and then divided — so its per-frame figures
#   came from different runs and its ratio carried that difference. A fixed count makes runs
#   comparable, and repeating gives a spread instead of a point.
#
#   `RAYLAND_S_SHIP_PRESENTED=1` on S disables the presented-buffer exclusion inside the same
#   binary, so the A/B attributes a traffic difference to that change rather than to a rebuild.
#
# ------------------------------------------------------------------------------------------
# ------------------------------------------------------------------------------------------
# EVERY RUN REPORTS ITS OWN CONTAMINATION, and this is not optional decoration
#   On 2026-08-30 the repository owner pointed out that vkcube stalls for seconds whenever the
#   mouse enters or leaves its window, and that this invalidates any frame-rate figure taken while
#   somebody is using the machine. It does. Measured: with NO pointer movement, 439 frames in 20 s
#   and **zero** 100 ms buckets containing no frames. With five synthetic pointer moves, a clean
#   500 ms hole. During such a stall the application's main thread sits in `hrtimer_nanosleep` —
#   Mesa's `vn_relax` ring-wait backoff — so a brief hiccup is amplified into a long one.
#
#   An averaged fps number hides that completely: 21.9 and 20.6 fps look like the same measurement.
#   So every run here samples the presented-frame count at a fixed 100 ms cadence and reports the
#   number of EMPTY buckets and the longest run of them. **0 empty buckets means the figure is
#   clean; anything else means it is not, and the run says so rather than averaging it away.**
#
#   THE SAMPLER RUNS ON C, IN BOTH TOPOLOGIES. An earlier attempt polled C's log over one ssh per
#   sample, which paced at ~250 ms rather than 100 ms and produced a two-machine number that could
#   not be compared with a loopback one. That result was discarded. Sampling locally on C and
#   fetching the timeline once at the end makes the two topologies measure the same thing.
#
# *** vkcube MUST RUN WITH `--gpu_number 0` *** — it defaults to the discrete NVIDIA GPU, whose
#   real vkQueueSubmit returns VK_ERROR_DEVICE_LOST (7/14 runs, against 0/10 on the Intel iGPU).
#   That cost this project three days and was never a Rayland bug. See DIARY.md, 2026-07-26.
#
# `VN_DEBUG=no_abort` is deliberately absent: Mesa's stall abort is the stall detector.
#
# PROCESS HYGIENE: every process is killed by the PID this script captured, never by name.
#
# Usage:
#   scripts/wp0-soak.sh                        # rate mode: $RUNS runs, report rate + modes
#   RUNS=60 APP_SECONDS=30 scripts/wp0-soak.sh
#   MODE=traffic RUNS=5 FRAMES=200 scripts/wp0-soak.sh            # exclusion ON
#   MODE=traffic RUNS=5 FRAMES=200 SHIP_PRESENTED=1 scripts/wp0-soak.sh   # exclusion OFF (A/B)
set -uo pipefail

PORT="${PORT:-9407}"
MODE="${MODE:-rate}"                       # rate | traffic
RUNS="${RUNS:-40}"
APP_SECONDS="${APP_SECONDS:-30}"           # rate mode: how long the app runs
FRAMES="${FRAMES:-200}"                    # traffic mode: fixed frame count (vkcube --c)
MIN_ATTACHES="${MIN_ATTACHES:-$((APP_SECONDS * 2))}"   # the 2 fps floor; see the header
SHIP_PRESENTED="${SHIP_PRESENTED:-}"       # set to 1 to disable the presented-buffer exclusion
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/debug"
BIN_EARLY="$BIN"

# C_HOST empty runs the application and rayland-c on THIS machine over loopback.
#
# Why that mode exists: the failure rate and the traffic ratio this harness measures are properties of
# the relay and the replay, not of the wire between two hosts, so loopback measures them faithfully.
# It was added on a day `apollo` was unreachable, and the alternative was measuring nothing. A run's
# topology is recorded in its output directory so no figure can later be mistaken for the other case.
#
# What loopback does NOT measure, and must not be quoted for: anything about latency, bandwidth, or a
# genuinely weak C machine.
C_HOST="${C_HOST-apollo}"
if [ -n "$C_HOST" ]; then
  C_IP="$(getent ahostsv4 "$C_HOST" | awk '{print $1; exit}')"
  S_IP="${S_IP:-$(ip -4 route get "$C_IP" | grep -oP 'src \K[\d.]+')}"
  # Run a command on machine C. One definition, so the two topologies cannot drift apart.
  on_c() { ssh "$C_HOST" "$@"; }
  C_BIN=/tmp
else
  S_IP=127.0.0.1
  on_c() { bash -c "$@"; }
  C_BIN="$BIN_EARLY"
fi
SOCK=/tmp/rl-soak.sock
WL_SOCK=/tmp/rl-soak-wayland.sock
WESTON_SOCKET="${WESTON_SOCKET:-wl-soak1}"
VKCUBE="${VKCUBE:-/usr/bin/vkcube}"
[ -n "${C_HOST-apollo}" ] && VKCUBE=/tmp/vkcube
OUT="${OUT:-/tmp/wp0-soak-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"

# ---- Pre-flight: refuse to measure through someone else's leftovers ------------------------
# A previous sweep killed mid-run (a `timeout`, a Ctrl-C) leaves its daemon and app alive on C,
# still holding the vtest socket. The next run then quietly collides with them and produces a
# stalled-looking result that is an artefact of the harness, not of the code under test — which
# happened once and cost a sweep. Fail loudly instead, and name the PIDs so a human can end them
# deliberately; this script will not pattern-kill processes it did not start.
leftovers=$(on_c 'ps -o pid=,cmd= -C vkcube -C rayland-c 2>/dev/null')
if [ -n "$leftovers" ]; then
  echo "REFUSING TO RUN: processes from an earlier sweep are still alive on $C_HOST:"
  echo "$leftovers"
  echo "End them by PID, then re-run."
  exit 1
fi

# `NO_BUILD=1` skips the rebuild, so an A/B can put a deliberately-chosen pair of binaries in place
# and alternate between them. Without it the two arms of a comparison would both be whatever the tree
# currently compiles to, which is the same arm twice.
if [ -z "${NO_BUILD:-}" ]; then
  echo "### building ###"
  CARGO_TARGET_DIR="$TARGET_DIR" cargo build -q -p rayland-c -p rayland-s || exit 1
fi
if [ -n "$C_HOST" ]; then
  scp -q "$BIN/rayland-c" /usr/bin/vkcube "$C_HOST:/tmp/" || exit 1
  ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/vkcube'
fi

# ---- The headless compositor, started once and shared by every run --------------------------
# Started here rather than per-run because compositor startup is ~5 s and is not what is being
# measured; a fresh one per run would triple the soak's wall-clock for no added coverage.
if ! pgrep -f "weston --backend=headless.*--socket=$WESTON_SOCKET" >/dev/null; then
  echo "### starting headless weston on $WESTON_SOCKET (Mesa/Intel, GL renderer) ###"
  rm -f "$XDG_RUNTIME_DIR/$WESTON_SOCKET" "$XDG_RUNTIME_DIR/$WESTON_SOCKET.lock"
  __EGL_VENDOR_LIBRARY_FILENAMES=/usr/share/glvnd/egl_vendor.d/50_mesa.json \
    setsid weston --backend=headless --renderer=gl --width=1280 --height=1024 \
    --socket="$WESTON_SOCKET" --idle-time=0 --no-config > "$OUT/weston.log" 2>&1 &
  sleep 6
fi
grep -q 'Using GL renderer' "$OUT/weston.log" 2>/dev/null || \
  echo "NOTE: weston already running from an earlier invocation; its log is not in $OUT"

pass=0; fail=0
declare -A modes=()
: > "$OUT/runs.tsv"
printf 'run\tverdict\tmodes\tattaches\tc2s_bytes\ts2c_bytes\tframes\tstall_gaps\tlongest_ms\tframe_gaps\tmedian_gap_ms\n' >> "$OUT/runs.tsv"

for run in $(seq 1 "$RUNS"); do
  RD="$OUT/run$run"; mkdir -p "$RD"
  S_PID=""; C_PID=""; A_PID=""
  cleanup_run() {
    [ -n "$A_PID" ] && on_c "kill $A_PID 2>/dev/null" || true
    [ -n "$C_PID" ] && on_c "kill $C_PID 2>/dev/null" || true
    [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  }

  # S. RAYLAND_S_EVENT_LOG arms the return-path witness, which is where event drops are visible.
  # `env` rather than a bare assignment prefix: the optional SHIP_PRESENTED expands to nothing in the
  # common case, and an empty word in an assignment prefix ends the prefix and makes bash read the next
  # word as the command name. `env` swallows the empty expansion harmlessly.
  env WAYLAND_DISPLAY="$WESTON_SOCKET" RAYLAND_S_EVENT_LOG=1 RAYLAND_C1_NO_PRESENT=1 \
    ${SHIP_PRESENTED:+RAYLAND_S_SHIP_PRESENTED=1} ${LINK_LOG:+RAYLAND_S_REPLY_LOG=1} ${LOCKSTAT:+RAYLAND_S_LOCKSTAT=1} \
    ${POLL_US:+RAYLAND_S_PROGRESS_POLL_US=$POLL_US} \
    RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" > "$RD/s.log" 2>&1 &
  S_PID=$!
  sleep 3
  if ! kill -0 "$S_PID" 2>/dev/null; then
    fail=$((fail+1)); modes[early_exit]=$(( ${modes[early_exit]:-0} + 1 ))
    printf '%s\tFAIL\tearly_exit(S)\t0\t0\t0\t0\n' "$run" >> "$OUT/runs.tsv"; continue
  fi

  # `C_WRAP` prefixes the rayland-c launch, so a run can put the daemon under a CPU quota or a
  # `taskset`. Why that exists: two mechanism fixes on 2026-08-31 cut C's send cost 5.4x and S's
  # lock-held time 8.3x and moved the frame rate by nothing measurable, on a laptop where C has CPU to
  # spare. The claim those fixes make is about a machine where C does NOT — the riscv64 board that
  # manages 5 fps — and starving C here is the safe way to test that claim without installing a Vulkan
  # stack onto a board whose root filesystem is 92% full. Example:
  #   C_WRAP='systemd-run --user --scope -q -p CPUQuota=15% -p AllowedCPUs=0 --'
  C_PID=$(on_c "rm -f $SOCK $WL_SOCK
    RAYLAND_WP_LOG=1 RAYLAND_C1_METRICS=1 ${LINK_LOG:+RAYLAND_C1_LINK_LOG=1} \
    ${PARK_US:+RAYLAND_C1_PARK_SLEEP_US=$PARK_US} ${RELAXSTAT:+RAYLAND_C1_RELAXSTAT=1} \
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK \
    RAYLAND_C1_WAYLAND_DISPLAY=$WL_SOCK nohup ${C_WRAP:-} $C_BIN/rayland-c >/tmp/soak-c.log 2>&1 & echo \$!")
  sleep 3

  # The application always runs FREE. vkcube's own frame-limited mode (`--c N`) was tried for the
  # fixed-count runs and **stalls at a single attach** under this path, while the same binary
  # free-running sustains ~20 fps — measured, `--c 60` gave 1 attach and never exited. That is a real
  # observation about `--c` and is reported rather than chased; here it only means the frame count has
  # to be imposed by this harness instead, by stopping the app once C's log shows enough attaches.
  APP_ARGS="--gpu_number 0"
  A_PID=$(on_c "export XDG_RUNTIME_DIR=/run/user/\$(id -u)
    WAYLAND_DISPLAY=$WL_SOCK VN_DEBUG=vtest \
    VN_PERF=no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT nohup $VKCUBE $APP_ARGS >/tmp/soak-app.log 2>&1 & echo \$!")



  if [ "$MODE" = traffic ]; then
    # Impose the frame count from here: poll C's own request trace until it has forwarded $FRAMES
    # surface attaches, then stop. Bounded, so a stalled run ends the wait instead of hanging the sweep
    # — and a run that hits the bound without reaching $FRAMES is scored on its actual attach count,
    # which the per-run table records, rather than being silently averaged in as if it were complete.
    for _ in $(seq 1 90); do
      n=$(on_c "grep -c 'forward obj 3 opcode 1 ' /tmp/soak-c.log 2>/dev/null || echo 0")
      [ "${n:-0}" -ge "$FRAMES" ] && break
      sleep 2
    done
  else
    sleep "$APP_SECONDS"
  fi

  # Stop the app first so C's session ends cleanly and reports its byte counters.
  on_c "kill $A_PID 2>/dev/null" || true
  sleep 4
  if [ -n "$C_HOST" ]; then
    scp -q "$C_HOST:/tmp/soak-c.log" "$RD/c.log" 2>/dev/null || true
    scp -q "$C_HOST:/tmp/soak-app.log" "$RD/app.log" 2>/dev/null || true
    scp -q "$C_HOST:/tmp/soak-timeline.dat" "$RD/timeline.dat" 2>/dev/null || true
  else
    cp /tmp/soak-c.log "$RD/c.log" 2>/dev/null || true
    cp /tmp/soak-app.log "$RD/app.log" 2>/dev/null || true
    cp /tmp/soak-timeline.dat "$RD/timeline.dat" 2>/dev/null || true
  fi
  cleanup_run
  sleep 1

  # ---- Score the run against the failure definition -----------------------------------------
  m=""
  inv=$(grep -c 'Invalid ObjectId' "$RD/s.log" "$RD/c.log" 2>/dev/null | awk -F: '{s+=$2} END{print s+0}')
  # Every drop except the accepted keymap fd. `grep -v` before counting, so the exclusion is visible.
  # Drops, excluding two cases — and the exclusions are the part to scrutinise, because a soak's
  # exclusions are where it quietly stops measuring anything.
  #
  #   1. `carries-fd wl_keyboard.keymap` — a file descriptor cannot cross a network and no application
  #      here blocks on it. Known, accepted, and a separate task.
  #   2. **Anything after the application began destroying its objects.** Run 13 of the 60-run soak
  #      failed on two drops that landed after every object had been destroyed and immediately before
  #      "session ended cleanly": S had events in flight for objects the app had legitimately finished
  #      with. That is benign, and counting it made `1 in 60` mean something it did not.
  #
  # The teardown guard is keyed on the app's OWN behaviour, and on the destruction of a
  # **session-lifetime** object specifically — NOT on elapsed time, and NOT on "any destruction".
  #
  # The first draft keyed on the first `objects-` line of any kind, and was badly wrong: a
  # `wl_callback` is destroyed after every frame, so in the archived run that motivated this guard the
  # first destruction was at line 62 of 4086 and the guard would have excused essentially the entire
  # run. That is precisely the failure the commissioning brief warned about — the exclusion is where a
  # soak quietly stops measuring anything — and it was caught only by running the guard against that
  # archived log instead of trusting it.
  #
  # In that same log, by interface: wl_callback destroyed 471 times, while xdg_surface, xdg_toplevel
  # and wl_surface were destroyed exactly ONCE each, in a burst at the very end. Those are created once
  # and destroyed once, so their destruction is a real shutdown signal that per-frame churn cannot
  # trigger. A time-based guard would silently excuse real late failures; this one excuses only events
  # arriving after the application has demonstrably begun dismantling its window.
  teardown_line=$(grep -nE 'objects- app_obj=[0-9]+ (xdg_toplevel|xdg_surface|wl_surface)' \
                  "$RD/c.log" 2>/dev/null | head -1 | cut -d: -f1)
  if [ -n "$teardown_line" ]; then
    # Only drops BEFORE the first destruction count. S's log has no teardown marker of its own, so its
    # drops are counted in full — which is the conservative choice.
    c_drops=$(head -n "$teardown_line" "$RD/c.log" 2>/dev/null | grep -c 'wp-event.\[C\] drop:' || true)
  else
    c_drops=$(grep -c 'wp-event.\[C\] drop:' "$RD/c.log" 2>/dev/null || true)
  fi
  s_drops=$(grep -h 'wp-event.\[S\] drop:' "$RD/s.log" 2>/dev/null \
            | grep -vc 'carries-fd wl_keyboard.keymap' || true)
  drops=$(( ${c_drops:-0} + ${s_drops:-0} ))
  proto=$(grep -chE 'protocol error|panicked|PANICKED' "$RD/s.log" 2>/dev/null | awk '{s+=$1} END{print s+0}')
  attaches=$(grep -c 'forward obj 3 opcode 1 ' "$RD/c.log" 2>/dev/null || echo 0)
  c2s=$(grep -oE 'c2s_total_bytes=[0-9]+' "$RD/c.log" 2>/dev/null | tail -1 | cut -d= -f2)
  s2c=$(grep -oE 's2c_total_bytes=[0-9]+' "$RD/c.log" 2>/dev/null | tail -1 | cut -d= -f2)
  built=$(grep -c 'built wl_buffer' "$RD/s.log" 2>/dev/null || echo 0)

  # ---- Contamination: empty 100 ms buckets, and the longest run of them ------------------------
  # A clean run has ZERO. Anything else means something outside the relay stole time during the
  # measurement — a human at the machine being the known case — and the fps figure understates.
  # Reported, never corrected for: a contaminated number is not a number to be adjusted, it is a
  # number to be re-taken.
  # Derive the frame timeline from the timestamps already in the proxy log — no sampler, no runtime
  # cost, exact inter-frame gaps. A "stall" is an inter-frame gap far longer than the run's own median,
  # which is the definition that does not need a hard-coded frame rate.
  awk '/forward obj 3 opcode 1 / { if (match($0, /t_ns=[0-9]+/)) print substr($0, RSTART+5, RLENGTH-5) }' \
    "$RD/c.log" > "$RD/timeline.dat" 2>/dev/null || true
  if [ -s "$RD/timeline.dat" ]; then
    # Count buckets in which the frame counter did not advance, and the longest run of them —
    # **only up to the last sample that advanced**. The sampler deliberately outlives the
    # application (it cannot know when the app will be stopped), so the timeline always ends in a
    # tail of non-advancing samples that is the run ending, not a stall. Scoring that tail is not a
    # subtle error: on a completely idle machine it reported a 3,400 ms stall in a run that the
    # hand-rolled timeline had measured as perfectly clean. Two passes, because the cut-off is only
    # known once the whole file has been read.
    # Two passes: the median gap, then how many gaps exceed 10x it. 10x is deliberately loose — the
    # stalls this exists to catch were 500-3400 ms against a ~45 ms median, i.e. 10-75x, while ordinary
    # scheduling jitter is well under 2x. A tighter threshold would report jitter as contamination.
    read -r empty longest samples medgap < <(awk '
      NR == FNR { if (FNR > 1) g[++k] = $1 - prev; prev = $1; next }
      FNR == 1 {
        asort(g); med = (k ? g[int(k/2)+1] : 0)
      }
      { }
      END {
        for (i = 1; i <= k; i++) {
          if (med > 0 && g[i] > 10 * med) { e++; if (g[i] > worst) worst = g[i] }
        }
        printf "%d %d %d %d\n", e + 0, worst / 1000000, k + 0, med / 1000000
      }' "$RD/timeline.dat" "$RD/timeline.dat")
  else
    empty=-1; longest=-1; samples=0; medgap=0
  fi

  [ "${inv:-0}" -gt 0 ] && m="$m invalid_object"
  [ "${drops:-0}" -gt 0 ] && m="$m event_drop"
  [ "${proto:-0}" -gt 0 ] && m="$m protocol"
  if [ "$MODE" != traffic ] && [ "${attaches:-0}" -lt "$MIN_ATTACHES" ]; then m="$m liveness"; fi

  if [ -z "$m" ]; then
    pass=$((pass+1)); verdict=PASS
  else
    fail=$((fail+1)); verdict=FAIL
    for mode in $m; do modes[$mode]=$(( ${modes[$mode]:-0} + 1 )); done
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$run" "$verdict" "${m:--}" "${attaches:-0}" \
    "${c2s:-0}" "${s2c:-0}" "${built:-0}" "${empty:-0}" "${longest:-0}" "${samples:-0}" "${medgap:-0}" >> "$OUT/runs.tsv"
  # The median gap is reported alongside, because "no stalls" and "fast" are different claims: a
  # uniformly loaded machine produces a low frame rate with no abnormal gap at all, and this check
  # would call that clean. It is clean *of stalls*; the median says whether it was also fast.
  clean_note="clean, median gap ${medgap}ms"
  [ "${empty:-0}" -gt 0 ] && clean_note="CONTAMINATED ${empty}/${samples} gaps, longest ${longest}ms, median ${medgap}ms"
  [ "${empty:-0}" -lt 0 ] && clean_note="no timeline"
  echo "run $run/$RUNS: $verdict${m:+ ($m)} attaches=$attaches c2s=${c2s:-?} s2c=${s2c:-?} [$clean_note]"
done

echo
echo "############ $MODE result ############"
echo "runs=$RUNS pass=$pass fail=$fail"
if [ "$fail" -eq 0 ]; then
  # Rule of three: zero failures in n runs bounds the rate at 3/n with 95% confidence. It does
  # NOT establish zero, and saying so here keeps the next reader from claiming that it does.
  echo "0 failures in $RUNS runs -> rate < $(awk -v n="$RUNS" 'BEGIN{printf "%.2f", 300/n}')% at 95% confidence (rule of three)"
else
  echo "failure modes:"; for k in "${!modes[@]}"; do echo "  $k: ${modes[$k]}"; done
fi
# ---- The contamination verdict for the sweep as a whole -----------------------------------------
# Printed last and unconditionally, because an fps or traffic figure quoted without it is exactly the
# kind of number this project has had to retract before.
awk -F'\t' 'NR>1 { e+=$8; if ($8>0) bad++; if ($9>worst) worst=$9; n++ }
  END { if (n==0) exit;
        if (e==0) printf "CONTAMINATION: none — %d/%d runs had no abnormal inter-frame gaps. Figures are clean.\n", n, n;
        else printf "CONTAMINATION: %d of %d runs disturbed (%d empty buckets total, longest stall %d ms).\n  Frame-rate figures from this sweep UNDERSTATE. Re-take on an idle machine.\n", bad, n, e, worst }' \
  "$OUT/runs.tsv"
echo "per-run table: $OUT/runs.tsv"
