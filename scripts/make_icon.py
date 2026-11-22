"""Generate assets/app.ico: a sun inside a full brightness ring.

Pure stdlib. Uses 4x supersampling, matching scripts/preview_tray.py and the
Rust tray icon in src/main.rs so the exe icon and tray icon share one design.
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "assets", "app.ico")
SUN = (255, 205, 72)


def pixels(size):
    scale = 4
    n = size * scale
    center = (n - 1) / 2.0 + 0.5
    core_r = size * 0.175 * scale
    ray_in, ray_out = size * 0.205 * scale, size * 0.335 * scale
    ring_r = size * 0.415 * scale
    ring_hw = max(0.9, size * 0.035) * scale

    grid = [[(0, 0, 0, 0) for _ in range(n)] for _ in range(n)]
    for yy in range(n):
        for xx in range(n):
            dx = xx + 0.5 - center
            dy = yy + 0.5 - center
            d = math.hypot(dx, dy)
            if d <= core_r:
                grid[yy][xx] = SUN + (255,)
                continue
            hit = False
            for k in range(8):
                a = k * math.pi / 4
                ux, uy = math.cos(a), math.sin(a)
                proj = dx * ux + dy * uy
                perp = -dx * uy + dy * ux
                if ray_in <= proj <= ray_out:
                    q = (proj - ray_in) / (ray_out - ray_in)
                    if abs(perp) <= (1.9 - 1.15 * q) * scale:
                        hit = True
                        break
            if hit:
                grid[yy][xx] = SUN + (255,)
            elif abs(d - ring_r) <= ring_hw:
                grid[yy][xx] = SUN + (255,)  # full ring for the static app icon

    result = []
    for y in range(size):
        row = []
        for x in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(scale):
                for sx in range(scale):
                    p = grid[y * scale + sy][x * scale + sx]
                    for i in range(4):
                        acc[i] += p[i]
            row.append(tuple(v // (scale * scale) for v in acc))
        result.append(row)
    return result


def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


def png(size):
    raw = b"".join(b"\x00" + b"".join(bytes(p) for p in row) for row in pixels(size))
    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main():
    sizes = [16, 24, 32, 48, 64]
    images = [png(s) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries = b""
    for size, image in zip(sizes, images):
        entries += struct.pack("<BBBBHHII", size, size, 0, 0, 1, 32, len(image), offset)
        offset += len(image)
    os.makedirs(os.path.dirname(OUT), exist_ok=True)
    with open(OUT, "wb") as f:
        f.write(header + entries + b"".join(images))
    print(f"wrote {OUT} ({os.path.getsize(OUT)} bytes)")


if __name__ == "__main__":
    main()
