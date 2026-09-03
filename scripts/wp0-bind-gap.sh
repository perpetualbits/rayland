#!/usr/bin/env bash
#
# THE BIND-GAP REPORT: what does this application ask for that WP0 does not offer?
# =============================================================================================
#
# Run an application against a real compositor, capture every `wl_registry.bind`, and diff that
# against the globals WP0 advertises. The output is the honest answer to "what is this program
# missing when we run it over Rayland", for ANY program -- including ones nobody has tried.
#
# WHY THIS RATHER THAN A LIST OF SUPPORTED INTERFACES. This repository already learned that a test
# enumerating the things it supports cannot find the one you forgot: S's registry test asserted all
# eleven listed names resolve, and passed for a day while `wl_shm` was missing. The only symptom was
# a cursor that never appeared, and the only detector was a human looking at a screen. A diff against
# what a real application actually asks for is a question the code cannot answer wrongly by omission.
#
# THREE TRAPS, all paid for in real sessions:
#
#   1. RUN AGAINST A FULL COMPOSITOR, NEVER HEADLESS WESTON. Headless weston advertises no `wl_seat`,
#      so every sweep this project ran was structurally blind to an entire class of interface. This
#      script refuses to run against a display whose name looks headless.
#   2. THE TRACE FORMAT VARIES. libwayland prints `wl_registry@2` in some builds and `wl_registry#2`
#      in others -- both were seen on this machine on 2026-09-02, and the first attempt at this
#      capture extracted nothing because of it. The pattern accepts both, and the script FAILS LOUDLY
#      on zero matches: an empty gap must mean "nothing missing", never "the parser did not match".
#   3. SOME APPLICATIONS CAPTURE THEIR OWN STDERR. `rt` calls
#      `crashlog::capture_stderr_if_not_a_tty()` and redirects to ~/.cache/rt/stderr.log, so a
#      redirected run looks like it produced no protocol trace at all. If the trace is empty, look
#      there before concluding the application binds nothing. `APP_TRACE=` points at such a file.
#
# Usage:
#   APP=~/.cargo/bin/solarsim scripts/wp0-bind-gap.sh
#   APP=/usr/bin/vkcube SECONDS_TO_RUN=6 scripts/wp0-bind-gap.sh
#   APP_TRACE=~/.cache/rt/stderr.log scripts/wp0-bind-gap.sh    # trace an app already run
set -u
SECONDS_TO_RUN="${SECONDS_TO_RUN:-8}"
OUT="${OUT:-/tmp/wp0-bind-gap}"
mkdir -p "$OUT"

# Derive what WP0 offers FROM THE CODE, never from a copy kept in this script.
BIN="${BIN:-./target/debug/rayland-c}"
if [ ! -x "$BIN" ]; then
  echo "### building rayland-c to ask it what it advertises ###"
  cargo build -q -p rayland-c || { echo "ABORTING: cannot build rayland-c" >&2; exit 1; }
fi
"$BIN" --print-globals > "$OUT/wp0-offers.txt" || { echo "ABORTING: --print-globals failed" >&2; exit 1; }
[ -s "$OUT/wp0-offers.txt" ] || { echo "ABORTING: rayland-c advertises nothing; that cannot be right" >&2; exit 1; }

if [ -n "${APP_TRACE:-}" ]; then
  cp "$APP_TRACE" "$OUT/trace.log" || { echo "ABORTING: cannot read APP_TRACE=$APP_TRACE" >&2; exit 1; }
  echo "### using an existing trace: $APP_TRACE ###"
else
  APP="${APP:?set APP=/path/to/the/application, or APP_TRACE=/path/to/an/existing/trace}"
  [ -n "${WAYLAND_DISPLAY:-}" ] || { echo "ABORTING: no WAYLAND_DISPLAY; run this from the live session." >&2; exit 1; }
  case "$WAYLAND_DISPLAY" in
    *soak*|*headless*) echo "ABORTING: WAYLAND_DISPLAY=$WAYLAND_DISPLAY looks headless. See trap 1." >&2; exit 1 ;;
  esac
  echo "### tracing $(basename "$APP") against $WAYLAND_DISPLAY for ${SECONDS_TO_RUN}s ###"
  WAYLAND_DEBUG=1 timeout "$SECONDS_TO_RUN" "$APP" >"$OUT/trace.log" 2>&1
fi

# Accept BOTH `@` and `#` object separators. See trap 2.
grep -oE 'wl_registry[#@][0-9]+\.bind\([0-9]+, *"[a-zA-Z_0-9]+", *[0-9]+' "$OUT/trace.log" \
  | sed -E 's/.*"([a-zA-Z_0-9]+)", *([0-9]+)/\1 \2/' | sort -u > "$OUT/bound.txt"

if [ ! -s "$OUT/bound.txt" ]; then
  echo "ABORTING: extracted ZERO binds from $OUT/trace.log." >&2
  echo "  An empty gap must mean 'nothing missing', never 'the parser did not match'." >&2
  echo "  Check the trace format (trap 2), and whether the application captured its own stderr" >&2
  echo "  (trap 3 -- rt writes to ~/.cache/rt/stderr.log; re-run with APP_TRACE= pointing there)." >&2
  exit 1
fi

echo "### this application binds $(wc -l < "$OUT/bound.txt") globals; WP0 offers $(wc -l < "$OUT/wp0-offers.txt") ###"
echo "### what WP0 does not offer ###"
gap=0
while read -r name version; do
  if ! grep -qx "$name" "$OUT/wp0-offers.txt"; then
    printf "  MISSING  %-40s (app wants v%s)\n" "$name" "$version"
    gap=$((gap + 1))
  fi
done < "$OUT/bound.txt"
[ "$gap" -eq 0 ] && echo "  (none)"
echo "### gap: $gap interface(s) ###"
