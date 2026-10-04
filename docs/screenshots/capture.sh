#!/bin/sh
set -eu
chrome="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
profile=""
trap 'if [ -n "$profile" ]; then rm -rf "$profile"; fi' EXIT HUP INT TERM
for theme in light dark; do
    profile=$(mktemp -d)
    scheme="--blink-settings=preferredColorScheme=1"
    if [ "$theme" = dark ]; then scheme="--force-dark-mode --blink-settings=preferredColorScheme=0"; fi
    output="docs/img/dashboard-$theme.png"
    rm -f "$output"
    python3 - "$output" "$chrome" --user-data-dir="$profile" --headless=new --hide-scrollbars \
        --no-first-run --no-default-browser-check --disable-extensions --disable-gpu \
        --disable-background-networking $scheme --window-size=1440,700 \
        --force-device-scale-factor=2 --virtual-time-budget=2000 \
        --screenshot="$output" http://127.0.0.1:7799/ <<'PY'
import os
from pathlib import Path
import signal
import subprocess
import sys

process = subprocess.Popen(sys.argv[2:], start_new_session=True)
try:
    code = process.wait(timeout=15)
    assert code == 0, f"Chrome exited {code}"
except subprocess.TimeoutExpired:
    # Chrome on macOS may linger after writing its screenshot.
    os.killpg(process.pid, signal.SIGTERM)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
output = Path(sys.argv[1])
assert output.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"), "Missing PNG"
assert output.stat().st_size < 600_000, "PNG exceeds 600 KB"
PY
    rm -rf "$profile"
    profile=""
done
