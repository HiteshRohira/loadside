#!/bin/bash
# Build and install Loadside to ~/Applications, put `loadside` on PATH and
# register the background service. The previous version stays in place until
# the new one is ready, and is restored if registering the service fails.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
"$ROOT/scripts/build-app.sh"

DEST="$HOME/Applications/Loadside.app"
NEW="$HOME/Applications/.Loadside.app.new"
OLD="$HOME/Applications/.Loadside.app.old"
mkdir -p "$HOME/Applications"
rm -rf "$NEW" "$OLD"
cp -R "$ROOT/dist/Loadside.app" "$NEW"
codesign --verify --deep "$NEW" >/dev/null 2>&1 || { echo "error: the new build doesn't pass codesign --verify" >&2; rm -rf "$NEW"; exit 1; }

# Quit the app and stop the old daemon so their binaries can be replaced.
osascript -e 'tell application id "dev.loadside.app" to quit' >/dev/null 2>&1 || true
# A daemon blocked on a Keychain dialog can't answer `daemon stop`, so don't
# wait on it forever: ask, then terminate whatever is still running.
if [ -x "$DEST/Contents/MacOS/loadside-cli" ]; then
    "$DEST/Contents/MacOS/loadside-cli" daemon stop >/dev/null 2>&1 &
    STOP_PID=$!
fi
for _ in $(seq 1 20); do pgrep -f "$DEST/Contents/MacOS/loadsided" >/dev/null || break; sleep 0.25; done
if pgrep -f "$DEST/Contents/MacOS/loadsided" >/dev/null; then
    pkill -TERM -f "$DEST/Contents/MacOS/loadsided" || true
    for _ in $(seq 1 20); do pgrep -f "$DEST/Contents/MacOS/loadsided" >/dev/null || break; sleep 0.25; done
    pkill -KILL -f "$DEST/Contents/MacOS/loadsided" || true
fi
[ -n "${STOP_PID:-}" ] && kill "$STOP_PID" 2>/dev/null || true

[ -e "$DEST" ] && mv "$DEST" "$OLD"
mv "$NEW" "$DEST"

if ! "$DEST/Contents/MacOS/loadside-cli" daemon install --bin "$DEST/Contents/MacOS/loadsided"; then
    echo "Registering the background service failed; restoring the previous version." >&2
    rm -rf "$DEST"
    if [ -e "$OLD" ]; then
        mv "$OLD" "$DEST"
        "$DEST/Contents/MacOS/loadside-cli" daemon install --bin "$DEST/Contents/MacOS/loadsided" || true
    fi
    exit 1
fi
rm -rf "$OLD"

mkdir -p "$HOME/.local/bin"
ln -sf "$DEST/Contents/MacOS/loadside-cli" "$HOME/.local/bin/loadside"
echo
echo "Installed: $DEST"
echo "CLI:       ~/.local/bin/loadside  (make sure ~/.local/bin is on your PATH)"
echo "Open the app with: open \"$DEST\""
