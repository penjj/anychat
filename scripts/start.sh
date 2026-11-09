#!/usr/bin/env bash
# Run the app. It starts hidden and never steals focus: summon with ⌥Space or the tray.
set -euo pipefail
cd "$(dirname "$0")/../src-tauri"
RUST_LOG=info ./target/debug/anychat "$@"
