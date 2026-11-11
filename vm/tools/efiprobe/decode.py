"""decode.py SCREENSHOT.png: read efiprobe's bit strips (exc.rs) back into numbers.
The screenshot must be the guest's full-size screen (e.g. from VNC), not scaled."""
import sys
from PIL import Image

im = Image.open(sys.argv[1]).convert("RGB")
# the VNC console may scale the guest's 1920x1080 screen
k = im.size[0] / 1920
x0, y0, sq, gap = 40, 100, 24, 32
names = sys.argv[2].split(",") if len(sys.argv) > 2 else ["magic", "ELR", "ESR", "FAR", "LR", "SP", "X0", "X1"]
for row, name in enumerate(names):
    v = 0
    for bit in range(64):
        r, g, b = im.getpixel((int((x0 + bit * sq + 10) * k), int((y0 + row * gap + 10) * k)))
        v = (v << 1) | (1 if r > 200 and g > 200 and b > 200 else 0)
    print(f"{name:6} {v:#018x}")
