#!/bin/bash
# Render docs/icons/src/<style>.svg into assets/AppIcon.icns (and a 1024 PNG).
# Needs a Chromium-based browser for SVG filters (Helium or Chrome).
# usage: scripts/render-icon.sh halo|dropin|stack|glass
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STYLE="${1:?style}"
SVG="$ROOT/docs/icons/src/$STYLE.svg"
BROWSER="${BROWSER:-}"
for b in "/Applications/Helium.app/Contents/MacOS/Helium" "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"; do
    [ -z "$BROWSER" ] && [ -x "$b" ] && BROWSER="$b"
done
[ -n "$BROWSER" ] || { echo "error: needs Helium or Google Chrome to render SVG" >&2; exit 1; }
TMP="$(mktemp -d)"
printf '<html><body style="margin:0;background:transparent">%s</body></html>' "$(cat "$SVG")" > "$TMP/icon.html"
"$BROWSER" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
    --window-size=1024,1024 --screenshot="$TMP/icon.png" "file://$TMP/icon.html" >/dev/null 2>&1
mkdir -p "$TMP/AppIcon.iconset" "$ROOT/assets"
for base in 16 32 128 256 512; do
    sips -z $base $base "$TMP/icon.png" --out "$TMP/AppIcon.iconset/icon_${base}x${base}.png" >/dev/null
    sips -z $((base*2)) $((base*2)) "$TMP/icon.png" --out "$TMP/AppIcon.iconset/icon_${base}x${base}@2x.png" >/dev/null
done
iconutil -c icns "$TMP/AppIcon.iconset" -o "$ROOT/assets/AppIcon.icns"
cp "$TMP/icon.png" "$ROOT/assets/AppIcon.png"
rm -rf "$TMP"
echo "assets/AppIcon.icns <- $STYLE"
