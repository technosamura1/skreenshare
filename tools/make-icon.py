#!/usr/bin/env python3
"""Draws the screenshare logo and writes everything the build needs into assets/:
icon.png (256), icon.ico (16..256), icon128.rgba (window icon, raw RGBA) and icon.res (Win32 resources for the exe).
Run: python3 tools/make-icon.py   (needs Pillow)."""
import io, struct, math
from PIL import Image, ImageDraw, ImageFilter

S = 1024  # drawn big, shrunk with LANCZOS = clean edges

def lerp(a, b, t): return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))

def draw():
    # rounded square with a diagonal blue -> violet gradient
    grad = Image.new("RGB", (S, S))
    px = grad.load()
    c0, c1 = (0x3b, 0x82, 0xf6), (0x8b, 0x5c, 0xf6)
    for y in range(S):
        for x in range(S):
            px[x, y] = lerp(c0, c1, (x + y) / (2 * S - 2))
    mask = Image.new("L", (S, S), 0)
    ImageDraw.Draw(mask).rounded_rectangle((16, 16, S - 17, S - 17), radius=230, fill=255)
    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)
    # soft highlight on the top edge
    hl = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    ImageDraw.Draw(hl).rounded_rectangle((16, 16, S - 17, S // 2), radius=230, fill=(255, 255, 255, 34))
    hl.putalpha(Image.composite(hl.getchannel("A"), Image.new("L", (S, S), 0), mask))
    img = Image.alpha_composite(img, hl)
    d = ImageDraw.Draw(img)
    white = (255, 255, 255, 255)
    # monitor: rounded frame, stand
    d.rounded_rectangle((190, 250, 834, 660), radius=60, outline=white, width=44)
    d.rounded_rectangle((470, 690, 554, 780), radius=10, fill=white)
    d.rounded_rectangle((350, 770, 674, 822), radius=26, fill=white)
    # "broadcast" arcs inside the screen, from a dot at the lower left
    cx, cy = 318, 585
    d.ellipse((cx - 36, cy - 36, cx + 36, cy + 36), fill=white)
    for r in (115, 200, 285):
        box = (cx - r, cy - r, cx + r, cy + r)
        d.arc(box, 270, 360, fill=white, width=40)
        # round caps
        for ang in (270, 360):
            a = math.radians(ang)
            x, y = cx + r * math.cos(a), cy + r * math.sin(a)
            d.ellipse((x - 20, y - 20, x + 20, y + 20), fill=white)
    return img

big = draw()
sizes = [256, 48, 32, 24, 16]
pngs = {}
for n in sizes:
    im = big.resize((n, n), Image.LANCZOS)
    b = io.BytesIO(); im.save(b, "PNG"); pngs[n] = b.getvalue()
big.resize((256, 256), Image.LANCZOS).save("assets/icon.png")
open("assets/icon128.rgba", "wb").write(big.resize((128, 128), Image.LANCZOS).tobytes())

# .ico with PNG-compressed images
ico = struct.pack("<HHH", 0, 1, len(sizes))
off = 6 + 16 * len(sizes)
data = b""
for n in sizes:
    ico += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(pngs[n]), off + len(data))
    data += pngs[n]
open("assets/icon.ico", "wb").write(ico + data)

# .res: RT_GROUP_ICON (14) id 1 + RT_ICON (3) ids 1..n, neutral language
def res_entry(rtype, rid, payload):
    def idnum(v): return struct.pack("<HH", 0xFFFF, v)
    body = idnum(rtype) + idnum(rid)
    body += b"\x00" * ((-len(body)) % 4)
    body += struct.pack("<IHHII", 0, 0x1030, 0, 0, 0)  # DataVersion, MemoryFlags(MOVEABLE|PURE|DISCARDABLE), Lang, Version, Characteristics
    hsize = 8 + len(body)
    out = struct.pack("<II", len(payload), hsize) + body + payload
    out += b"\x00" * ((-len(out)) % 4)
    return out

res = struct.pack("<II", 0, 32) + struct.pack("<HHHHIHHII", 0xFFFF, 0, 0xFFFF, 0, 0, 0, 0, 0, 0)  # empty header entry
group = struct.pack("<HHH", 0, 1, len(sizes))
icons = b""
for i, n in enumerate(sizes, 1):
    group += struct.pack("<BBBBHHIH", n % 256, n % 256, 0, 0, 1, 32, len(pngs[n]), i)
    icons += res_entry(3, i, pngs[n])
res += icons + res_entry(14, 1, group)
open("assets/icon.res", "wb").write(res)
print("ok", {n: len(p) for n, p in pngs.items()})
