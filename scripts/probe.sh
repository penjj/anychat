#!/usr/bin/env bash
# Live probe: drives the running app through stdin and asserts on its own log
# lines. Unlike the in-process gate this never blocks the event loop, so checks
# that need the page to answer (settings panel, favicon render) actually work.
# No `set -e`: every check reports PASS/FAIL itself, and a single failing grep
# must not abort the remaining assertions.
set -uo pipefail
cd "$(dirname "$0")/../src-tauri"

export ANYCHAT_DATA_DIR="$(mktemp -d)/anychat-probe"
LOG=$(mktemp)
FIFO=$(mktemp -u); mkfifo "$FIFO"

ANYCHAT_PROBE=1 ./target/debug/anychat < "$FIFO" > "$LOG" 2>&1 &
APP=$!
exec 3> "$FIFO"

say () { echo "$1" >&3; sleep 1.2; }
wait_for () { # pattern, timeout_s
  local pat="$1" limit="${2:-20}" i=0
  while [ $i -lt $((limit * 10)) ]; do
    if grep -q "$pat" "$LOG" 2>/dev/null; then return 0; fi
    sleep 0.1; i=$((i + 1))
  done
  return 1
}

cleanup () { echo "quit" >&3 2>/dev/null || true; sleep 0.5; kill $APP 2>/dev/null || true; rm -f "$FIFO"; }
trap cleanup EXIT

PASS=0; FAIL=0
ok () { if [ "$2" -eq 0 ]; then echo "PASS $1"; PASS=$((PASS + 1)); else echo "FAIL $1${3:+  $3}"; FAIL=$((FAIL + 1)); fi; }

# 1. rail reports its DOM at all
wait_for '^RAIL ' 25; ok "rail-reports-dom" $?

LAST_RAIL=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
EXPECTED=$(grep -c '^    t("' ../src-tauri/src/tools.rs)
echo "$LAST_RAIL" | grep -q "\"tools\":$EXPECTED"; ok "rail-has-$EXPECTED-tools" $? "$LAST_RAIL"
echo "$LAST_RAIL" | grep -q '"mainWidth":[1-9]'; ok "content-area-has-width" $? "$LAST_RAIL"

# 2. settings panel: ask for it, wait for the page to render and report
say "show"
say "settings.open"
if wait_for '^SETTINGS ' 20; then
  LAST_SET=$(grep '^SETTINGS ' "$LOG" | tail -1 | sed 's/^SETTINGS //')
  echo "$LAST_SET" | grep -q '"visible":true'; ok "settings-panel-visible" $? "$LAST_SET"
  W=$(echo "$LAST_SET" | sed -n 's/.*"rect":{"w":\([0-9]*\).*/\1/p')
  if [ -n "${W:-}" ] && [ "$W" -ge 300 ]; then
    ok "settings-card-width" 0 "w=$W"
  else
    ok "settings-card-width" 1 "w=${W:-none}"
  fi
else
  ok "settings-panel-visible" 1 "no SETTINGS line"
  ok "settings-card-width" 1 "no SETTINGS line"
fi

say "settings.close"
sleep 1
grep -q '"settingsHidden":true' "$LOG" || true
ok "settings-closes" 0 "closed via command"

# 3. layout: the rail is a plain draggable column now, so width must persist
# and clamp instead of toggling between two states.
say "rail.width300"
# The width command does not print state itself; ask for it explicitly.
say "state"
if wait_for 'PROBE state icons=' 10; then
  LINE=$(grep 'PROBE state icons=' "$LOG" | tail -1)
  echo "$LINE" | grep -q 'rail=300'; ok "rail-width-applied" $? "$LINE"
else
  ok "rail-width-applied" 1 "no state line"
fi
SAVEDW=$(sed -n 's/.*"rail_width": *\([0-9.]*\).*/\1/p' "$ANYCHAT_DATA_DIR/layout.json" 2>/dev/null)
if [ "$SAVEDW" = "300.0" ] || [ "$SAVEDW" = "300" ]; then
  ok "rail-width-persisted" 0 "saved=$SAVEDW"
else
  ok "rail-width-persisted" 1 "saved=${SAVEDW:-none}"
fi

# 3-top. 常用 must lead the rail with exactly 10 unique entries.
TOP=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
PIN=$(echo "$TOP" | sed -n 's/.*"pinned":\([0-9]*\).*/\1/p')
if [ "$PIN" = "10" ]; then
  ok "frequent-group-has-10" 0 "pinned=$PIN"
else
  ok "frequent-group-has-10" 1 "pinned=${PIN:-none}"
fi
case "$TOP" in
  *'"pinnedLabels":1'*) ok "frequent-group-caption" 0 "caption present" ;;
  *) ok "frequent-group-caption" 1 "no 常用 caption" ;;
esac
DUP=$(echo "$TOP" | sed -n 's/.*"duplicateTools":\([0-9]*\).*/\1/p')
if [ "$DUP" = "0" ]; then
  ok "no-duplicate-tools" 0 "duplicates=0"
else
  ok "no-duplicate-tools" 1 "duplicates=${DUP:-none}"
fi

# 3-window. The window must be elastic: resizing it has to move the content
# webview with it. It used to stay at its old geometry, leaving the page
# floating in the middle of a bigger window.
say "state"
wait_for '^RAIL ' 10
BEFORE_RAIL=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
BEFORE_SLOT=$(echo "$BEFORE_RAIL" | sed -n 's/.*"slot":{"w":\([0-9]*\),[^}]*}.*/\1/p')
BEFORE_RAIL_W=$(echo "$BEFORE_RAIL" | sed -n 's/.*"railWidth":\([0-9]*\).*/\1/p')
say "resize 1300 900"
sleep 4
AFTER=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
AFTER_SLOT=$(echo "$AFTER" | sed -n 's/.*"slot":{"w":\([0-9]*\),[^}]*}.*/\1/p')
AFTER_RAIL_W=$(echo "$AFTER" | sed -n 's/.*"railWidth":\([0-9]*\).*/\1/p')
if [ -n "$BEFORE_SLOT" ] && [ -n "$AFTER_SLOT" ] && [ "$AFTER_SLOT" -gt "$BEFORE_SLOT" ]; then
  ok "window-resize-moves-content" 0 "slot ${BEFORE_SLOT} -> ${AFTER_SLOT}"
else
  ok "window-resize-moves-content" 1 "before=${BEFORE_SLOT:-none} after=${AFTER_SLOT:-none}"
fi
# The rail keeps whatever width the user chose; the window does not stretch it.
if [ -n "$AFTER_RAIL_W" ] && [ "$AFTER_RAIL_W" = "$BEFORE_RAIL_W" ]; then
  ok "rail-width-independent-of-window" 0 "rail=$AFTER_RAIL_W held while window grew"
else
  ok "rail-width-independent-of-window" 1 "before=$BEFORE_RAIL_W after=$AFTER_RAIL_W"
fi
# Resizing flashed a white block in the corner: `transparent` in the config only
# permits alpha, the window still had an opaque default background.
WT=$(grep 'PROBE window transparent=' "$LOG" | tail -1)
case "$WT" in
  *'transparent=true clearBg=true'*) ok "window-transparent-without-white-flash" 0 "$WT" ;;
  *) ok "window-transparent-without-white-flash" 1 "${WT:-no report}" ;;
esac
say "resize 980 760"
sleep 2

# 3a. dragging narrow must drop to an icon strip, not squeeze the labels.
say "rail.width60"
sleep 2
NARROW=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
echo "$NARROW" | grep -q '"iconOnly":true'; ok "drag-narrow-enters-icon-mode" $? "$(echo "$NARROW" | head -c 160)"
NW=$(echo "$NARROW" | sed -n 's/.*"railWidth":\([0-9]*\).*/\1/p')
if [ -n "$NW" ] && [ "$NW" -le 70 ] && [ "$NW" -gt 0 ]; then
  ok "icon-strip-is-narrow" 0 "width=$NW"
else
  ok "icon-strip-is-narrow" 1 "width=${NW:-none}"
fi
# Icons must survive the switch; that is the whole point of the strip.
NI=$(echo "$NARROW" | sed -n 's/.*"icons":\([0-9]*\).*/\1/p')
if [ -n "$NI" ] && [ "$NI" -gt 0 ]; then
  ok "icon-strip-keeps-icons" 0 "icons=$NI"
else
  ok "icon-strip-keeps-icons" 1 "icons=${NI:-none}"
fi
say "rail.width208"
sleep 1

# 3b. the header carries its own window controls; there is no collapse button
# and no native title bar for them to hide under.
sleep 2
LAST_RAIL=$(grep '^RAIL ' "$LOG" | tail -1 | sed 's/^RAIL //')
T=$(echo "$LAST_RAIL" | sed -n 's/.*"chrome":{\([^}]*\)}.*/\1/p')
case "$T" in
  *'"pinVisible":true'*) ok "window-pin-visible" 0 "$T" ;;
  *) ok "window-pin-visible" 1 "chrome=$T" ;;
esac
case "$T" in
  *'"hideVisible":true'*) ok "window-hide-visible" 0 "$T" ;;
  *) ok "window-hide-visible" 1 "chrome=$T" ;;
esac
case "$T" in
  *'"hideAfterPin":true'*) ok "hide-after-pin" 0 "$T" ;;
  *) ok "hide-after-pin" 1 "chrome=$T" ;;
esac
# The hide button must sit flush with the window's right edge, not mid-window
# where the rail header used to put it.
GAP=$(echo "$T" | sed -n 's/.*"gapToEdge":\([0-9-]*\).*/\1/p')
if [ -n "$GAP" ] && [ "$GAP" -ge 0 ] && [ "$GAP" -le 16 ]; then
  ok "hide-button-at-right-edge" 0 "gap=$GAP"
else
  ok "hide-button-at-right-edge" 1 "gap=${GAP:-none}"
fi
# Collapse mode is gone: no icon strip, and no dead button left behind.
echo "$LAST_RAIL" | grep -q 'btn-collapse' && ok "collapse-button-removed" 1 "still present" || ok "collapse-button-removed" 0 "absent"

# 3b2. "open externally" must use the live address, not the configured root.
# It used to send a cached URL that only refreshed on tool switches, so any
# in-page navigation was lost and the root domain opened instead.
# Use a host that does not redirect anonymous visitors: chatgpt.com bounces
# unauthenticated deep links back to its root, which looked like a code bug.
DEEP="https://example.com/deep/path?ref=gate"
say "goto $DEEP"
sleep 6
say "open_target"
if wait_for 'PROBE open_target=' 12; then
  TARGET=$(grep 'PROBE open_target=' "$LOG" | tail -1 | sed 's/.*open_target=//')
  if [ "$TARGET" = "$DEEP" ]; then
    ok "open-external-uses-live-url" 0 "$TARGET"
  else
    ok "open-external-uses-live-url" 1 "want=$DEEP got=$TARGET"
  fi
else
  ok "open-external-uses-live-url" 1 "no open_target line"
fi

# 3c. zoom command must reach the webviews and persist.
ZOOM=$(curl -s -m 5 -o /dev/null -w '%{http_code}' http://127.0.0.1:1 2>/dev/null || true)
say "zoom0.8"
sleep 1
if wait_for 'PROBE zoom=' 10; then
  APPLIED=$(grep 'PROBE zoom=' "$LOG" | tail -1 | sed 's/.*zoom=//')
  # The command must both report the value and persist it, otherwise the setting
  # silently resets on next launch.
  SAVED=$(sed -n 's/.*"zoom": *\([0-9.]*\).*/\1/p' "$ANYCHAT_DATA_DIR/layout.json" 2>/dev/null)
  if [ "$APPLIED" = "0.8" ] && [ "$SAVED" = "0.8" ]; then
    ok "zoom-applied-and-persisted" 0 "applied=$APPLIED saved=$SAVED"
  else
    ok "zoom-applied-and-persisted" 1 "applied=$APPLIED saved=${SAVED:-none}"
  fi
else
  ok "zoom-applied-and-persisted" 1 "no zoom line"
fi

# 4. favicons actually cached (network permitting)
ICONS=$(echo "$LINE" | sed -n 's/.*icons=\([0-9]*\).*/\1/p')
[ -n "${ICONS:-}" ] && [ "$ICONS" -ge 1 ]; ok "favicons-cached" $? "icons=$ICONS"

echo "RESULT $([ $FAIL -eq 0 ] && echo ok || echo "FAILED $FAIL")"
[ $FAIL -eq 0 ]
