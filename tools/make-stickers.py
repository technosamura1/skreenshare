# Packs animated Telegram stickers (.webm, VP9 with alpha) into assets/stickers/cats.zst for stickers.rs.
# Usage: python tools/make-stickers.py OUT.zst a.webm b.webm ...   (needs: pip install av zstandard pillow)
# Layout (before zstd): b"CAT1" | n u8 | per sticker: frames u8, frames × duration ms u16 |
#   then per sticker: frames × 48x48 BGRA, frames × 128x128 BGRA (premultiplied, cropped to the union of all
#   frames' visible pixels, centred on a square).
import struct, sys
import av, zstandard
from PIL import Image

SIZES = (48, 128)

def frames_of(path):
    c = av.open(path)
    s = c.streams.video[0]
    dec = av.CodecContext.create("libvpx-vp9", "r")  # the native vp9 decoder drops the alpha plane
    out = []
    for pkt in c.demux(s):
        for f in dec.decode(pkt):
            out.append((float(f.pts * s.time_base) if f.pts is not None else None, Image.fromarray(f.to_ndarray(format="rgba"), "RGBA")))
    total = float(c.duration / 1e6) if c.duration else None
    times = [t for t, _ in out]
    durs = []
    for i, t in enumerate(times):
        nxt = times[i + 1] if i + 1 < len(times) else total
        d = (nxt - t) if (t is not None and nxt is not None) else 1 / 25
        durs.append(max(20, min(2000, round(d * 1000))))
    return [im for _, im in out], durs

def main():
    out_path, files = sys.argv[1], sys.argv[2:]
    head = bytearray(b"CAT1") + bytes([len(files)])
    body = bytearray()
    for path in files:
        ims, durs = frames_of(path)
        box = None
        for im in ims:
            b = im.getchannel("A").point(lambda a: 255 if a > 8 else 0).getbbox()
            if b:
                box = b if box is None else (min(box[0], b[0]), min(box[1], b[1]), max(box[2], b[2]), max(box[3], b[3]))
        side = max(box[2] - box[0], box[3] - box[1])
        head += bytes([len(ims)]) + b"".join(struct.pack("<H", d) for d in durs)
        for size in SIZES:
            for im in ims:
                sq = Image.new("RGBA", (side, side), (0, 0, 0, 0))
                crop = im.crop(box)
                sq.alpha_composite(crop, ((side - crop.width) // 2, (side - crop.height) // 2))
                r = sq.resize((size, size), Image.LANCZOS)
                px = r.tobytes()
                for i in range(0, len(px), 4):
                    R, G, B, A = px[i], px[i + 1], px[i + 2], px[i + 3]
                    body += bytes((B * A // 255, G * A // 255, R * A // 255, A))
        print(path, len(ims), "frames", sum(durs), "ms")
    raw = bytes(head + body)
    open(out_path, "wb").write(zstandard.ZstdCompressor(level=19).compress(raw))
    print("raw", len(raw), "->", out_path)

main()
