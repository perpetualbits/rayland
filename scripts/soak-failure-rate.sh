#!/usr/bin/env bash
#
# MEASURE A CONFIGURATION'S FAILURE RATE over many real-network runs.
# =============================================================================================
#
# Why this exists: until 2026-07-27 the strongest statement this project could make about its own
# reliability was "10/10 clean", so a single observed failure had nothing to be read against — and
# one was briefly (and wrongly) taken as condemning Venus's feedback mechanisms. This harness
# produced the first real number: **0 failures in 480 runs of the shipping configuration**, i.e. a
# rate under 0.62% at 95% confidence (rule of three).
#
# It deliberately does NOT stop at the first failure: a rate needs its whole denominator. Cores are
# captured on C and post-mortemed as they appear, and the loop continues.
#
# Usage:
#   TRIES=400 scripts/soak-failure-rate.sh            # the shipping config (default)
#   TRIES=400 VN_PERF_SETTING=no_multi_ring,no_fence_feedback scripts/soak-failure-rate.sh
#
# That second form is THE NEXT CHEAP EXPERIMENT this project has queued: the semaphore/event/query
# feedback arm measured 1.23x faster and has exactly one unexplained failure against it (1/92). One
# night of this settles what argument could not.
#
# *** NINE DEFECTS THAT WOULD HAVE SILENTLY VOIDED THAT NIGHT, FOUND 2026-09-02, FIXED HERE. ***
# Each one produced a plausible-looking result for the wrong thing, which is the failure mode this
# project has now paid for more than any other. Kept written down because the fix is invisible once
# applied, and the next person to add a harness should recognise the shapes:
#
#   1. THE ARM WAS NOT SELECTABLE. This script read `VNPERF`; its own usage line above, both sibling
#      harnesses (`c2-icosa-*.sh`), OVERVIEW §6.2 and the diary all say `VN_PERF_SETTING`. The
#      documented invocation therefore set a variable nothing read, and the run would have spent the
#      night measuring the DEFAULT arm — all feedback off, the one already known clean through 480 —
#      and reported it as the feedback arm. It now reads `VN_PERF_SETTING`, and it HARD-FAILS if the
#      old name is set, because a silently-ignored `VNPERF=` is exactly the same bug wearing the
#      other name.
#
#   2. THE SOFTWARE WAS FIVE WEEKS OLD. `BIN` pointed at `~/.cache/rayland-c1-target`, a directory no
#      other harness writes and nothing here rebuilt: binaries dated 2026-07-27, i.e. 26 commits
#      behind on `rayland-c` and `rayland-s` alone — missing the forward-message coalescing and the
#      `reply_arena_fence_signaled` memchr rewrite, among others. Every sibling harness builds first
#      into the shared `/tmp/rayland-c1-target`. This one now does too. A failure-rate figure names a
#      configuration OF A BUILD; without the build it is not a measurement of anything shipping.
#
#   3. THE OUTPUT DID NOT SAY WHAT PRODUCED IT. A 400-run result that outlives the terminal it ran in
#      needs its provenance attached to it, not to the operator's memory. It now prints the git rev,
#      the working-tree state, the resolved arm, the S-side GPU pin and the binary timestamps before
#      the first run, and repeats the arm in the result line.
#
#   5. EVIDENCE WAS OVERWRITTEN AND LANDED IN THE REPO. See the LOG block below.
#
#   6. A FAILED DEPLOY WAS NOT CHECKED, so the soak measured a binary it had failed to send.
#
#   7. A `rayland-c` LEAKED ON C EVERY ITERATION — 400 over a night, and the cause of 6.
#
#   9. AN UNREACHABLE C WAS ALSO SCORED AS AN ARM FAILURE -- the same bug as 8 on the other side of
#      the link, found when it voided a 400-run control arm. See the preflight above the loop.
#
#   8. THE HARNESS MANUFACTURED FAILURES. An S that had not released its port made the next
#      iteration's S fail to bind, which was scored against the arm under test. See the block
#      above the loop; this is the one that can have corrupted numbers already recorded.
#
#   4. S's GPU WAS NOT PINNED, and this is the one that would have hurt most. See the S_ICD block
#      below: an unpinned dop561 can enumerate the NVIDIA card, which loses the device SILENTLY on
#      about half its runs. Here that lands as `frames != 120` — scored as a FAILURE, charged to
#      whichever arm was running. The soak exists to resolve a 1-in-92 failure; it cannot share a
#      denominator with a 1-in-2 confound.
#
# Needs: C reachable over ssh (default apollo), passwordless sudo on C to point core_pattern at a
# file (restored on exit, always), and S's GPU free. Neither is available away from the two-machine
# setup, so this is not something to run on a laptop.
# BASELINE: what is the failure rate of the SHIPPING configuration (all feedback off)?
#
# Nobody has measured it. The feedback experiment was condemned on 1 failure in 10 and then survived
# 82 clean runs (~1%), which 20 feedback-off runs cannot distinguish from zero. Without this number
# no single failure can be read against anything.
#
# Unlike the feedback hunt this does NOT stop at the first failure — a rate needs the whole
# denominator. Cores are captured and post-mortemed as they appear, and the loop continues.
set -u
C_HOST=apollo; PORT=9414
# *** S's address is PINNED TO THE WiFi LINK, DELIBERATELY, AND THAT IS NOT A BUG. ***
#
# dop561 has two addresses and `scripts/c1-sweep.sh` measured both in July:
#   192.168.1.192 -> WiFi.  RTT from apollo: avg 11.8 ms, max 91 ms, mdev 26 ms.
#   192.168.1.150 -> br0, a wired USB Ethernet adapter. RTT: avg 0.65 ms, mdev 0.18 ms.
#
# Six sibling harnesses derive this with `ip route get`, which lands on the WIRED address. This one
# must not, and the reason is comparability rather than habit: the 0-in-480 shipping-arm figure
# (2026-07-27, this harness's first commit, which carried this same literal) and the 0-in-400
# feedback-arm figure (2026-09-02) were BOTH taken over .192. Deriving the address would silently
# move future runs onto a link with 18x less latency and 140x less jitter, and the result could no
# longer be pooled with either. A "fix" that improves the harness and quietly invalidates its own
# history is a worse outcome than a hardcode.
#
# Note this makes both figures STRONGER than a wired run would have been. They are reliability
# numbers, not timing numbers, and zero failures across a jittery 11.8 ms / 91 ms path says more
# than zero across a 0.65 ms wire. What they cannot do is characterise the wired path, and they may
# not be pooled with anything measured on .150.
#
# `S_IP=192.168.1.150` runs the wired link on purpose. The provenance block prints whichever was
# used, with its measured RTT, so the link can never again be implicit -- it has bitten twice.
S_IP="${S_IP:-192.168.1.192}"

PROBE_PORT="${PROBE_PORT:-22}"
SOCK=/tmp/rl-base.sock
# Where per-run evidence lands. Defect 5: this used to be `$(dirname "$0")` — the repo's own
# `scripts/` directory — so a soak littered the working tree with `base-*.txt` and, worse, wrote
# EVERY run's S-side log to one `base-s.log` that each iteration truncated. A 400-run soak exists to
# catch rare failures; losing the S log of the failing run unless it happened to be the last one is
# the exact opposite of that. Logs now go outside the repo, one S log per run.
LOG="${LOG:-/tmp/rayland-soak}"
mkdir -p "$LOG"
TRIES="${TRIES:-60}"

# Refuse the old, silently-ignored spelling rather than defaulting past it. Sampling `${VNPERF+set}`
# BEFORE any assignment is the whole point: a guard placed after `VNPERF="${VNPERF:-...}"` can never
# fire, which is its own entry in this project's list of harnesses that measured nothing.
if [ -n "${VNPERF+set}" ]; then
  echo "REFUSING TO RUN: VNPERF is set, and this harness reads VN_PERF_SETTING (as its siblings and" >&2
  echo "the documents always did). Re-run with VN_PERF_SETTING=$VNPERF — running on would have" >&2
  echo "measured the default all-feedback-off arm and labelled it yours." >&2
  exit 2
fi
# Which Venus feedback mechanisms Mesa may use, as a VN_PERF value. The default is the SHIPPING
# configuration: every feedback off. `no_fence_feedback` is load-bearing in any arm — (c)2's
# completion barrier works by spotting the application's `vkGetFenceStatus` reply, and fence feedback
# removes that poll entirely (exit 134, zero frames, every time). The arm this harness exists to
# settle drops the other three: `no_multi_ring,no_fence_feedback`.
VN_PERF_SETTING="${VN_PERF_SETTING:-no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback}"

# *** PIN WHAT S's VULKAN ENUMERATES — defect 4, and the one that would have hurt most. ***
# This was the only two-machine harness not doing it (`milkv-demo.sh`, both `c2-icosa-*.sh`,
# `wp0-soak.sh` and `wp0-milkv-ab.sh` all do). dop561 has an Intel iGPU and an NVIDIA RTX A500 whose
# VK_ERROR_DEVICE_LOST is SILENT: buffers get created, a commit or two happens, nothing is presented,
# and no log on either side says why — measured 7 of 14 runs lost on NVIDIA against 0 of 10 on Intel.
# In THIS harness that lands as `frames != 120`, i.e. as a FAILURE charged to whichever arm was
# running. A soak whose entire purpose is to resolve a 1-in-92 failure cannot share its denominator
# with a ~50%-per-run GPU confound. `S_ICD=` (empty) restores full enumeration.
S_ICD="${S_ICD-/usr/share/vulkan/icd.d/intel_icd.json}"

# Build, into the same target directory every other harness uses, so what is soaked is what is in the
# tree right now. See defect 2 in the header for what pointing at a stale prebuilt cost.
TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/rayland-c1-target}"
BIN="$TARGET_DIR/release"
echo "### building rayland-c, rayland-s, rayland-icosa-cpu (release) ###"
CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release -q \
  -p rayland-c -p rayland-s -p rayland-icosa-cpu || exit 1

S_PID=""; fails=0; ok=0

restore() {
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  ssh "$C_HOST" 'kill "$(cat /tmp/rayland-c.pid 2>/dev/null)" 2>/dev/null; \
    sudo -n sh -c "cat /tmp/core_pattern.orig > /proc/sys/kernel/core_pattern"' 2>/dev/null || true
  echo "core_pattern restored on $C_HOST"
}
trap restore EXIT

# Provenance, printed before the first run so the log says what produced the number that follows.
# A failure rate names a configuration of a build; a bare "0 in 400" names neither.
echo "### PROVENANCE ###"
echo "  arm (VN_PERF): $VN_PERF_SETTING"
echo "  git:           $(git -C "$(dirname "$0")/.." rev-parse --short HEAD) on \
$(git -C "$(dirname "$0")/.." rev-parse --abbrev-ref HEAD)\
$(git -C "$(dirname "$0")/.." diff --quiet HEAD 2>/dev/null || echo ' (DIRTY WORKING TREE)')"
echo "  binaries:      $(date -r "$BIN/rayland-c" '+%F %T') rayland-c, \
$(date -r "$BIN/rayland-s" '+%F %T') rayland-s"
echo "  S ICD pin:     ${S_ICD:-<none: full enumeration>}"
echo "  C host:        $C_HOST      S listen: 0.0.0.0:$PORT      tries: $TRIES"
# THE LINK, measured at run time. Two experiments have now been misread for want of this line: the
# 0/480 and 400/400 could not be attributed to a link without archaeology, and a topology change
# under a running sweep voided a 400-run control arm. Print what was actually used.
_rtt=$(ssh -o BatchMode=yes "$C_HOST" "ping -c 5 -q -W 2 $S_IP 2>/dev/null | tail -1" 2>/dev/null)
echo "  S address:     $S_IP$([ "$S_IP" = 192.168.1.192 ] && echo '  (the address the 0/480 and 400/400 figures used)' || echo '  (NOT the .192 those figures used; not poolable with them)')"
echo "  C sees S at:   $(ssh -o BatchMode=yes "$C_HOST" 'echo $SSH_CLIENT' 2>/dev/null | awk '{print $1}')"
echo "  measured RTT:  ${_rtt:-<unavailable>}"
# CHARACTERISE THE LINK FROM THE MEASUREMENT, NOT FROM THE ADDRESS. July's numbers (c1-sweep.sh)
# were WiFi avg 11.8 ms / mdev 26 ms against wired avg 0.65 ms / mdev 0.18 ms, and on 2026-09-02
# .192 measured 0.80 ms / mdev 0.095 -- wired-class, on the address labelled WiFi. The topology has
# changed three times in one day; an address is no longer evidence of a link, so print what the
# wire actually did and say plainly when it disagrees with the label.
_avg=$(printf '%s' "${_rtt:-}" | sed -nE 's#.*= *[0-9.]+/([0-9.]+)/.*#\1#p')
if [ -n "$_avg" ]; then
  _kind=$(awk -v a="$_avg" 'BEGIN{ print (a < 3) ? "wired-class" : "WiFi-class" }')
  echo "  link profile:  $_avg ms avg -> $_kind (July: WiFi 11.8 ms avg / mdev 26; wired 0.65 / 0.18)"
  if [ "$S_IP" = 192.168.1.192 ] && [ "$_kind" = "wired-class" ]; then
    echo "  NOTE: .192 was WiFi in July and is measuring wired-class now. The address label is stale;"
    echo "        believe this RTT, not the label, when deciding what this run is poolable with."
  fi
fi
ssh "$C_HOST" '
  cat /proc/sys/kernel/core_pattern > /tmp/core_pattern.orig
  sudo -n sh -c "echo /tmp/cores/core.%e.%p > /proc/sys/kernel/core_pattern"
  mkdir -p /tmp/cores && rm -f /tmp/cores/core.*
'
# Retire any `rayland-c` a PREVIOUS invocation of this harness leaked, before deploying over it.
# Only ever by the exact PID that invocation recorded, and only after confirming that PID is still
# the binary we started — a recycled PID must never be killed. Never by name or pattern.
ssh "$C_HOST" '
  p=$(cat /tmp/rayland-c.pid 2>/dev/null) || true
  if [ -n "${p:-}" ]; then
    case "$(readlink /proc/$p/exe 2>/dev/null)" in
      /tmp/rayland-c|"/tmp/rayland-c (deleted)")
        echo "retiring leaked rayland-c from a previous run: pid $p"; kill "$p" 2>/dev/null ;;
    esac
  fi
  rm -f /tmp/rayland-c.pid
' || true
sleep 1

# *** DEFECT 6: A FAILED DEPLOY WAS NOT CHECKED. ***
# This scp had no error check, and it fails in practice: a leaked `rayland-c` on C holds the file
# open, so the copy dies with ETXTBSY ("dest open ... Failure") — and the soak then ran the loop
# against whatever binary happened to already be in /tmp, and reported it as a clean result for the
# build it had just made. Observed on 2026-09-02: "6 clean, 0 failed" against a binary the harness
# had failed to deliver. A measurement of software that was never deployed is worse than no
# measurement, because it looks like one.
if ! scp -q "$BIN/rayland-c" "$BIN/rayland-icosa-cpu" "$C_HOST:/tmp/"; then
  echo "ABORTING: could not deploy the binaries to $C_HOST. Anything measured now would be" >&2
  echo "whatever build is already there, not the one just compiled." >&2
  exit 1
fi
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-icosa-cpu'

# *** DEFECT 8, AND THE ONE MOST LIKELY TO HAVE CORRUPTED THE EXISTING NUMBERS. ***
# The loop used to end an iteration with `kill "$S_PID"; sleep 1` and begin the next by starting
# `rayland-s` and sleeping 3 — with nothing anywhere checking that the old S had exited, that its
# listening port had been released, or that the new S had come up at all.
#
# It does not always hold. Observed on 2026-09-02 in six runs: an S died on SIGSEGV during teardown
# and was still holding the port a second later, so the NEXT iteration's S exited immediately with
# "binding S's listen address 0.0.0.0:9414 ... Address already in use". C then had nothing to talk
# to, produced 0 frames, and the harness scored the attempt as a **FAILURE OF THE ARM UNDER TEST**.
#
# That is a false-failure generator sitting inside the one instrument this project uses to compare
# arms, and it is worth being explicit about the consequence: the feedback question rests entirely on
# "1 unexplained failure in 92 runs", and a lost session with no core and no explanation is exactly
# the shape this bug produces. It is NOT established that this caused that failure — the 1/92 was
# hunted with core capture armed and no core appeared, which is consistent with the S-side bind
# failure but does not prove it. It does mean the old denominators cannot settle a ~1% effect.
#
# So: wait for the port, verify S is actually listening, and treat a failure to start as a HARNESS
# abort rather than a data point. A harness may lose a run; it may never invent one.
# NOTE THE `u` IN `-lntu`, AND DO NOT DROP IT. S's transport is QUIC, so its listener is a **UDP**
# socket; the first version of these helpers used `ss -ltn` and matched nothing, ever. That failed
# safely only because the caller aborts rather than assuming — a version that assumed "not listening
# means free" would have started every S into a port the last one still held.
wait_for_port_free() {                     # bounded: ~6 s, the observed teardown is well under 1 s
  for _ in $(seq 1 60); do
    ss -lntuH "sport = :$PORT" 2>/dev/null | grep -q . || return 0
    sleep 0.1
  done
  return 1
}
wait_for_port_listening() {                # bounded: ~6 s, S normally binds in well under 1 s
  for _ in $(seq 1 60); do
    ss -lntuH "sport = :$PORT" 2>/dev/null | grep -q . && return 0
    sleep 0.1
  done
  return 1
}

# *** DEFECT 9: AN UNREACHABLE C WAS SCORED AS A FAILURE OF THE ARM. ***
# Defect 8 taught the loop to abort when *S* fails to start. It said nothing about *C*, and on
# 2026-09-02 that cost a whole 400-run control arm: the LAN was being migrated onto a VLAN under the
# run, apollo moved to 172.16.20.10/24 while S stayed on 192.168.1.0/24, and from attempt 36 onward C
# could not reach S at all. Thirteen attempts failed with "Could not resolve hostname apollo" and 345
# more ran `rayland-c` against an S it could not dial. Every one of those 358 was written down as a
# FAILURE OF THE SHIPPING CONFIGURATION -- a config with 480 prior clean runs -- producing the
# preposterous headline "35 clean, 365 failed". Only the absurdity of the number made anyone look.
#
# The rule this enforces is the same one defect 8 established and this file keeps having to relearn:
# **a harness may lose a run; it may never invent one.** Anything that is not the application's own
# verdict aborts the sweep instead of entering the denominator.
preflight() {
  # 1. Is C reachable at all, and is it the host we think it is?
  local tok
  tok=$(ssh -o BatchMode=yes -o ConnectTimeout=10 "$C_HOST" 'echo RAYLAND_PREFLIGHT_OK' 2>&1) || true
  case "$tok" in
    *RAYLAND_PREFLIGHT_OK*) ;;
    *) echo "ABORTING before the first run: cannot reach C ($C_HOST) over ssh." >&2
       echo "  ssh said: $tok" >&2; return 1 ;;
  esac
  # 2. Can C actually reach S on the port the relay will use? This is the check whose absence voided
  #    the 2026-09-02 control arm. A route can exist in one direction only; ssh working proves
  #    dop561 -> apollo, and says nothing about apollo -> dop561, which is the direction C dials.
  local probe
  probe=$(ssh -o BatchMode=yes "$C_HOST" "timeout 5 bash -c '</dev/tcp/$S_IP/$PROBE_PORT' 2>/dev/null && echo REACH_OK || echo REACH_FAIL" 2>/dev/null)
  case "$probe" in
    *REACH_OK*) ;;
    *) echo "ABORTING before the first run: C cannot reach S at $S_IP:$PROBE_PORT." >&2
       echo "  C dials S over the network; a one-way route (ssh works, the reverse does not) is" >&2
       echo "  exactly what voided the 2026-09-02 control arm. Fix the route, or set S_IP." >&2
       return 1 ;;
  esac
  return 0
}

# Run the preconditions BEFORE any attempt is scored. Placed here rather than earlier because bash
# needs the definition first -- an earlier draft called it above its own definition, which would have
# aborted every run with "preflight: command not found".
# Abort the sweep WITHOUT discarding what it already measured.
#
# The guards below stop rather than score, which is right -- a harness may lose a run, it may never
# invent one. But until 2026-09-02 they also `exit 1` straight past the RESULT line, so an abort at
# attempt 37 of 400 threw away 36 perfectly good results. Losing a run and losing the whole run are
# different failures, and only the first is acceptable. The partial tally is printed with its
# denominator made explicit, so it can never be mistaken for a completed sweep.
abort_sweep() {
  echo "$1" >&2
  echo >&2
  echo "PARTIAL RESULT [arm: $VN_PERF_SETTING]: $ok clean, $fails failed, out of $((ok + fails))" >&2
  echo "  SWEEP ABORTED at attempt $i of $TRIES -- this is NOT a completed run and must not be" >&2
  echo "  quoted as one. The attempts above it were measured; the rest were never attempted." >&2
  exit 1
}

preflight || exit 1

for i in $(seq 1 "$TRIES"); do
  keep_s_log=0                             # per-iteration; set if S's teardown dies unexpectedly
  ssh "$C_HOST" 'rm -rf /tmp/icosa-base && mkdir -p /tmp/icosa-base && rm -f /tmp/cores/core.*'
  if ! wait_for_port_free; then
    abort_sweep "ABORTING at attempt $i: port $PORT still held after the previous S was retired.
Scoring this as an arm failure would be inventing data; stopping instead."
  fi
  env ${S_ICD:+VK_ICD_FILENAMES=$S_ICD} \
    RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"$LOG/base-s-$i.log" 2>&1 &
  S_PID=$!
  if ! wait_for_port_listening || ! kill -0 "$S_PID" 2>/dev/null; then
    cat "$LOG/base-s-$i.log" >&2
    abort_sweep "ABORTING at attempt $i: rayland-s never started listening (its log is above)."
  fi
  sleep 2                                  # S is listening; give the engine a moment before C dials
  ssh "$C_HOST" "
    # *** ulimit BEFORE the daemon is launched, not after. ***
    # A process inherits its core limit at exec, and apollo's shell limit is 0. With this line
    # below the nohup (where it sat until 2026-09-02) rayland-c could never dump a core, by
    # construction, so 'no core was produced' said nothing about the daemon -- only about the
    # application, which is launched after this line and was always fine.
    # NOTE: no backticks anywhere in this remote block. It is a double-quoted ssh string, so bash
    # runs backticked text as a command substitution ON S before sending it; a markdown-style
    # markdown-style nohup in backticks executed nohup with no arguments. Found the same day.
    ulimit -c unlimited
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-base.log 2>&1 &
    echo \$! > /tmp/rayland-c.pid
    sleep 3
    VN_DEBUG=vtest VN_PERF=$VN_PERF_SETTING \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT timeout 200 /tmp/rayland-icosa-cpu /tmp/icosa-base >/dev/null 2>&1
    rc=\$?
    echo \"rc=\$rc frames=\$(ls /tmp/icosa-base/frame_*.png 2>/dev/null | wc -l) cores=\$(ls /tmp/cores/core.* 2>/dev/null | wc -l)\"
    # *** DEFECT 7: retire THIS iteration's daemon. ***
    # Nothing used to. Each iteration started a rayland-c and left it; only the last PID was ever in
    # the pid file, so the exit trap could retire exactly one and the rest accumulated on C — 400 of
    # them over a night, contending for the same /tmp/rl-base.sock, and one of them holding
    # /tmp/rayland-c open so the NEXT invocation's deploy failed (defect 6). Kill by the exact PID
    # this iteration recorded, and only after confirming it is still our binary.
    cpid=\$(cat /tmp/rayland-c.pid 2>/dev/null)
    case \"\$(readlink /proc/\$cpid/exe 2>/dev/null)\" in
      /tmp/rayland-c|'/tmp/rayland-c (deleted)') kill \"\$cpid\" 2>/dev/null ;;
    esac
    rm -f /tmp/rayland-c.pid
  " > "$LOG/base-$i.txt" 2>&1
  # Retire S and WAIT for it, rather than sleeping a second and hoping. `wait` also reaps the job,
  # which is what stops bash printing its exit signal into the middle of the results.
  kill "$S_PID" 2>/dev/null
  wait "$S_PID" 2>/dev/null; s_status=$?
  S_PID=""
  # An UNEXPECTED teardown signal is worth recording — rayland-s has a history of dying during
  # cleanup (the libepoxy SIGABRT) and a SIGSEGV teardown is what held the port in the case above.
  # 143 is SIGTERM, i.e. the `kill` on the line above doing its job, and noting it every iteration
  # would bury the interesting case in 400 lines of noise. A teardown signal does NOT make the run a
  # failure either way: the frames were already produced and counted before S was retired.
  if [ "$s_status" -gt 128 ] && [ "$s_status" -ne 143 ]; then
    echo "  note: rayland-s teardown at attempt $i exited on signal $((s_status - 128)) (kept: $LOG/base-s-$i.log)"
    keep_s_log=1
  fi
  line=$(grep -oE 'rc=[0-9]+ frames=[0-9]+ cores=[0-9]+' "$LOG/base-$i.txt" | tail -1)
  # No verdict line at all means the ssh or the remote shell failed, so the application never ran and
  # there is NOTHING to score. Defect 9: this used to fall through to `${fr:-0}` -> 0 -> "FAILURE".
  if [ -z "$line" ]; then
    sed 's/^/    /' "$LOG/base-$i.txt" >&2
    abort_sweep "ABORTING at attempt $i: no verdict from C -- the application never ran (C's output is above)."
  fi
  # C reaching S is a precondition, not a result. If S never saw a connection, the run measured the
  # network, not the arm.
  if ! grep -q 'C connected' "$LOG/base-s-$i.log" 2>/dev/null; then
    sed 's/^/    /' "$LOG/base-s-$i.log" >&2
    abort_sweep "ABORTING at attempt $i: C never connected to S ($S_IP:$PORT).
That is the network, not the configuration under test (S's log is above)."
  fi
  fr=$(echo "$line" | grep -oE 'frames=[0-9]+' | cut -d= -f2)
  co=$(echo "$line" | grep -oE 'cores=[0-9]+' | cut -d= -f2)
  if [ "${fr:-0}" -ne 120 ] || [ "${co:-0}" -gt 0 ]; then
    fails=$((fails+1))
    echo "FAILURE $fails at attempt $i: $line"
    # Back-trace against the binary that ACTUALLY dumped. core_pattern is core.%e.%p, so the
    # executable name is in the filename; hardcoding the fixture (as this did until 2026-09-02)
    # back-traces a `rayland-c` core against `rayland-icosa-cpu` and prints confident nonsense.
    [ "${co:-0}" -gt 0 ] && ssh "$C_HOST" 'c=$(ls -t /tmp/cores/core.* | head -1);
      e=$(basename "$c" | cut -d. -f2);
      exe=/tmp/$e; [ -x "$exe" ] || exe=$(command -v "$e" 2>/dev/null);
      echo "core=$c executable=${exe:-UNKNOWN}";
      [ -n "$exe" ] && gdb --batch -ex "thread apply all bt 25" "$exe" "$c" 2>&1 \
        || echo "no executable found for %e=$e; not back-tracing against the wrong binary"' \
      > "$LOG/base-bt-$i.txt" 2>&1
  else
    ok=$((ok+1))
    # A clean run's S log says nothing; keep only the interesting ones so the directory stays
    # readable over 400 runs. `keep_s_log` is set above when S died on an unexpected signal — that
    # run still produced its 120 frames, but the teardown crash is exactly what defect 8 was about.
    [ "${keep_s_log:-0}" -eq 1 ] || rm -f "$LOG/base-s-$i.log"
  fi
  [ $((i % 10)) -eq 0 ] && echo "progress: $i done, ok=$ok fails=$fails"
done
echo "RESULT [arm: $VN_PERF_SETTING]: $ok clean, $fails failed, out of $TRIES"
