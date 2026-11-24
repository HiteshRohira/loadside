#!/bin/bash
# Rebuild the app icon from docs/icons/final:
#   sideglide-layer.svg  glass panes on a transparent background (the artwork)
#   AppIcon.icon         Icon Composer document: light ceramic fill + that layer;
#                        macOS 26 derives the dark and tinted looks from it
# Outputs (committed, so building Loadside needs neither tool):
#   assets/Assets.car    compiled icon (light/dark/tinted on macOS 26+)
#   assets/AppIcon.icns  flattened fallback for macOS 14-15
# Needs a Chromium-based browser (SVG filters) and Xcode's actool.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/docs/icons/final"
BROWSER="${BROWSER:-}"
for b in "/Applications/Helium.app/Contents/MacOS/Helium" "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"; do
    [ -z "$BROWSER" ] && [ -x "$b" ] && BROWSER="$b"
done
[ -n "$BROWSER" ] || { echo "error: needs Helium or Google Chrome to render SVG" >&2; exit 1; }
TMP="$(mktemp -d)"
render() { # svg out.png
    printf '<html><body style="margin:0;background:transparent">%s</body></html>' "$(cat "$1")" > "$TMP/r.html"
    "$BROWSER" --headless=new --disable-gpu --hide-scrollbars --default-background-color=00000000 \
        --window-size=1024,1024 --screenshot="$2" "file://$TMP/r.html" >/dev/null 2>&1
}
render "$SRC/sideglide-layer.svg" "$TMP/layer.png"
render "$SRC/sideglide.svg" "$SRC/sideglide.png"
# The .icon canvas is the icon body itself (the system draws the shape).
python3 -I - "$TMP/layer.png" "$SRC/AppIcon.icon/Assets/panes.png" <<'PY'
import sys
from PIL import Image
Image.open(sys.argv[1]).convert('RGBA').crop((100, 100, 924, 924)).resize((1024, 1024), Image.LANCZOS).save(sys.argv[2])
PY
mkdir -p "$TMP/out" "$ROOT/assets"
xcrun actool "$SRC/AppIcon.icon" --compile "$TMP/out" --platform macosx --minimum-deployment-target 14.0 \
    --app-icon AppIcon --output-partial-info-plist "$TMP/partial.plist" >/dev/null
cp "$TMP/out/Assets.car" "$ROOT/assets/Assets.car"
cp "$TMP/out/AppIcon.icns" "$ROOT/assets/AppIcon.icns"
ICTOOL="$(dirname "$(xcrun --find actool)")/../../Applications/Icon Composer.app/Contents/Executables/ictool"
[ -x "$ICTOOL" ] || ICTOOL="/Applications/Xcode.app/Contents/Applications/Icon Composer.app/Contents/Executables/ictool"
if [ -x "$ICTOOL" ]; then
    "$ICTOOL" "$SRC/AppIcon.icon" --export-image --output-file "$ROOT/assets/AppIcon.png" --platform macOS --rendition Default --width 1024 --height 1024 --scale 1 >/dev/null
    "$ICTOOL" "$SRC/AppIcon.icon" --export-image --output-file "$ROOT/assets/AppIcon-dark.png" --platform macOS --rendition Dark --width 1024 --height 1024 --scale 1 >/dev/null
fi
rm -rf "$TMP"
echo "assets/Assets.car + AppIcon.icns rebuilt"
