# Menu bar template icon from the app icon: python3 tools/mkmenuicon.py
# -> assets/menubar.png (36x36 = 18 pt @2x). The cards' colored area is ink; the white gaps,
# the HM letters and the dots are see-through. menubar.rs loads it as a template image.
from PIL import Image
src = Image.open("assets/hypermac-1024.png").convert("RGBA")
w, h = src.size
mask = Image.new("L", (w, h), 0)
px, mp = src.load(), mask.load()
for y in range(h):
    for x in range(w):
        r, g, b, a = px[x, y]
        # ink: opaque and colored (not white): white is min channel high
        ink = a > 128 and min(r, g, b) < 200
        mp[x, y] = 255 if ink else 0
box = mask.getbbox()
mask = mask.crop(box)
side = max(mask.size)
sq = Image.new("L", (side, side), 0)
sq.paste(mask, ((side - mask.size[0]) // 2, (side - mask.size[1]) // 2))
small = sq.resize((36, 36), Image.LANCZOS)
out = Image.new("RGBA", (36, 36), (0, 0, 0, 0))
out.putalpha(small)
out.save("assets/menubar.png")
