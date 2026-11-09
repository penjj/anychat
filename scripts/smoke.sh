#!/usr/bin/env bash
# Acceptance gate: builds, runs the headless self-check, prints PASS/FAIL, exits 0 on success.
# Hermetic: a fresh ANYCHAT_DATA_DIR so a developer's saved layout cannot leak in.
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"

echo "== build =="
cargo build 2>&1 | tail -3

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
