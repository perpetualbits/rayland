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
C_HOST=apollo; S_IP=192.168.1.192; PORT=9414
BIN=/home/roland/.cache/rayland-c1-target/release
SOCK=/tmp/rl-base.sock
LOG="$(dirname "$0")"
TRIES="${TRIES:-60}"
VNPERF="${VNPERF:-no_multi_ring,no_fence_feedback,no_semaphore_feedback,no_event_feedback,no_query_feedback}"
S_PID=""; fails=0; ok=0

restore() {
  [ -n "$S_PID" ] && kill "$S_PID" 2>/dev/null || true
  ssh "$C_HOST" 'kill "$(cat /tmp/rayland-c.pid 2>/dev/null)" 2>/dev/null; \
    sudo -n sh -c "cat /tmp/core_pattern.orig > /proc/sys/kernel/core_pattern"' 2>/dev/null || true
  echo "core_pattern restored on $C_HOST"
}
trap restore EXIT

echo "### VN_PERF=$VNPERF ###"
ssh "$C_HOST" '
  cat /proc/sys/kernel/core_pattern > /tmp/core_pattern.orig
  sudo -n sh -c "echo /tmp/cores/core.%e.%p > /proc/sys/kernel/core_pattern"
  mkdir -p /tmp/cores && rm -f /tmp/cores/core.*
'
scp -q "$BIN/rayland-c" "$BIN/rayland-icosa-cpu" "$C_HOST:/tmp/"
ssh "$C_HOST" 'chmod +x /tmp/rayland-c /tmp/rayland-icosa-cpu'

for i in $(seq 1 "$TRIES"); do
  ssh "$C_HOST" 'rm -rf /tmp/icosa-base && mkdir -p /tmp/icosa-base && rm -f /tmp/cores/core.*'
  RAYLAND_C1_NO_PRESENT=1 RAYLAND_C1_S_LISTEN="0.0.0.0:$PORT" "$BIN/rayland-s" >"$LOG/base-s.log" 2>&1 &
  S_PID=$!; sleep 3
  ssh "$C_HOST" "
    RAYLAND_C1_S_ADDR=$S_IP:$PORT RAYLAND_C1_SOCKET=$SOCK nohup /tmp/rayland-c >/tmp/rayland-c-base.log 2>&1 &
    echo \$! > /tmp/rayland-c.pid
    sleep 3
    ulimit -c unlimited
    VN_DEBUG=vtest VN_PERF=$VNPERF \
    VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/virtio_icd.json VTEST_SOCKET_NAME=$SOCK \
    env -u VK_LOADER_DRIVERS_SELECT timeout 200 /tmp/rayland-icosa-cpu /tmp/icosa-base >/dev/null 2>&1
    echo \"rc=\$? frames=\$(ls /tmp/icosa-base/frame_*.png 2>/dev/null | wc -l) cores=\$(ls /tmp/cores/core.* 2>/dev/null | wc -l)\"
  " > "$LOG/base-$i.txt" 2>&1
  kill "$S_PID" 2>/dev/null; S_PID=""; sleep 1
  line=$(grep -oE 'rc=[0-9]+ frames=[0-9]+ cores=[0-9]+' "$LOG/base-$i.txt" | tail -1)
  fr=$(echo "$line" | grep -oE 'frames=[0-9]+' | cut -d= -f2)
  co=$(echo "$line" | grep -oE 'cores=[0-9]+' | cut -d= -f2)
  if [ "${fr:-0}" -ne 120 ] || [ "${co:-0}" -gt 0 ]; then
    fails=$((fails+1))
    echo "FAILURE $fails at attempt $i: $line"
    [ "${co:-0}" -gt 0 ] && ssh "$C_HOST" 'c=$(ls -t /tmp/cores/core.* | head -1); gdb --batch -ex "thread apply all bt 25" /tmp/rayland-icosa-cpu "$c" 2>&1' > "$LOG/base-bt-$i.txt" 2>&1
  else
    ok=$((ok+1))
  fi
  [ $((i % 10)) -eq 0 ] && echo "progress: $i done, ok=$ok fails=$fails"
done
echo "BASELINE RESULT: $ok clean, $fails failed, out of $TRIES"
