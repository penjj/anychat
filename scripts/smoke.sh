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
# 3000ms is a ceiling with room for a cold cache on a slow machine, and far
# below the 5400ms the fixed animation enforced.
if [ -n "$HANDOVER" ] && [ "$HANDOVER" -lt 3000 ]; then
  echo "PASS startup-handover-early  ${HANDOVER}ms"
else
  echo "FAIL startup-handover-early  ${HANDOVER:-no handover line}ms"
fi

echo "== self-check =="
export ANYCHAT_DATA_DIR="$(mktemp -d)/anychat-gate"
ANYCHAT_SMOKE=1 ./target/debug/anychat 2>&1 | grep -viE "IMKCFRunLoop|mach port" > /tmp/anychat-smoke.txt || true
python3 - <<'PY'
import json, sys, pathlib
txt = pathlib.Path('/tmp/anychat-smoke.txt').read_text()
# Pick the SMOKE line specifically: the log also carries RAIL/SETTINGS reports and
# window-server noise, so a bare find() landed on the wrong JSON blob.
# The report is pretty-printed across many lines, so parse it with raw_decode
# starting right after the marker instead of assuming a single line.
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
