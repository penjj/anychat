#!/usr/bin/env bash
# Acceptance gate: builds, runs the headless self-check, prints PASS/FAIL, exits 0 on success.
# Hermetic: a fresh ANYCHAT_DATA_DIR so a developer's saved layout cannot leak in.
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"

echo "== build =="
cargo build 2>&1 | tail -3

# Launch-to-usable. The splash used to run a fixed 5.4s film regardless of
# readiness, which read as a slow start when the app itself was ready in under
# one second. This measures the handover the user actually waits for.
echo "== startup handover =="
export ANYCHAT_DATA_DIR="$(mktemp -d)/anychat-boot"
BOOT_LOG=$(mktemp)
./target/debug/anychat > "$BOOT_LOG" 2>&1 &
BOOT_PID=$!
HANDOVER=""
for _ in $(seq 1 200); do
  HANDOVER=$(sed -n 's/.*handing over at \([0-9]*\)ms.*/\1/p' "$BOOT_LOG" | head -1)
  [ -n "$HANDOVER" ] && break
  sleep 0.05
done
for _ in $(seq 1 10); do kill -0 "$BOOT_PID" 2>/dev/null || break; sleep 0.2; done
kill "$BOOT_PID" 2>/dev/null || true
for _ in $(seq 1 10); do kill -0 "$BOOT_PID" 2>/dev/null || break; sleep 0.2; done
kill -9 "$BOOT_PID" 2>/dev/null || true
wait "$BOOT_PID" 2>/dev/null || true
rm -f "$BOOT_LOG"
# Two failure modes, opposite directions.
#
# Too early means the splash was cut off before its film played, which was the
# bug that made the launch look like nothing happened: the window is hidden
# until handover, so closing the splash early shows a blank screen, not a faster
# one. Too late means a start slow enough to be worth fixing.
#
# The film is 3s; the window plus page load adds a few hundred ms.
# A ceiling that survives a slow shared runner without accepting a regression.
# The bug this guards is the splash film being cut short, which is under 2600;
# the upper bound catches "the app now waits on something it did not before".
# 12s: the animation is 3s, and the rest is webview start plus page load, which
# measured 11.4s on a cold hosted runner and under 5s locally.
HANDOVER_CEILING="${HANDOVER_CEILING:-12000}"
HANDOVER_FAIL=0
if [ -z "$HANDOVER" ]; then
  echo "FAIL startup-handover  no handover line"
  HANDOVER_FAIL=1
elif [ "$HANDOVER" -lt 2600 ]; then
  echo "FAIL startup-handover  ${HANDOVER}ms, splash cut before the film finished"
  HANDOVER_FAIL=1
elif [ "$HANDOVER" -gt "$HANDOVER_CEILING" ]; then
  echo "FAIL startup-handover  ${HANDOVER}ms, over the ${HANDOVER_CEILING}ms ceiling"
  HANDOVER_FAIL=1
else
  echo "PASS startup-handover  ${HANDOVER}ms"
fi

echo "== self-check =="
export ANYCHAT_DATA_DIR="$(mktemp -d)/anychat-gate"
# Not /tmp: it does not exist on Windows, and this gate runs there.
SMOKE_OUT="$(pwd)/target/anychat-smoke.txt"
ANYCHAT_SMOKE=1 ./target/debug/anychat 2>&1 | grep -viE "IMKCFRunLoop|mach port" > "$SMOKE_OUT" || true
# `set -e` would abort here on a failing self-check, which is why the status is
# captured with `||` instead of `$?` -- the latter is unreachable in that case.
SELF_CHECK=0
SMOKE_OUT="$SMOKE_OUT" python3 - <<'PY' || SELF_CHECK=$?
import json, os, sys, pathlib
txt = pathlib.Path(os.environ['SMOKE_OUT']).read_text()
marker = txt.rfind('SMOKE ')
if marker < 0:
    print('no SMOKE output; raw log:')
    print(txt[:2000])
    sys.exit(1)
d, _ = json.JSONDecoder().raw_decode(txt[marker + 6:].lstrip())
for s in d['steps']:
    print(('PASS ' if s['pass'] else 'FAIL ') + s['name'] + ('' if s['pass'] else '  ' + s['detail'][:200]))
print('RESULT', 'ok' if d['ok'] else f"FAILED {d['failures']}")
sys.exit(0 if d['ok'] else 1)
PY
if [ "$HANDOVER_FAIL" -ne 0 ] || [ "$SELF_CHECK" -ne 0 ]; then
  exit 1
fi
exit 0
