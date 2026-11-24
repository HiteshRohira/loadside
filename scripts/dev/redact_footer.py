"""Paint over the account email in the sidebar footer of Loadside window screenshots.

usage: python3 redact_footer.py in.png out.png
Works on 2x window captures (2080x1360). The footer's second line (the email)
sits at roughly y 1310..1346; each row is filled with the sidebar colour sampled
just right of the text so the vibrancy gradient is preserved.
"""
import sys
from PIL import Image

src, dst = sys.argv[1], sys.argv[2]
im = Image.open(src).convert("RGBA")
w, h = im.size
scale = w / 2080
y0, y1 = int(1310 * scale), int(1346 * scale)
x0, x1, xs = int(12 * scale), int(340 * scale), int(348 * scale)
px = im.load()
for y in range(y0, min(y1, h)):
    c = px[xs, y]
    for x in range(x0, x1):
        px[x, y] = c
im.save(dst)
