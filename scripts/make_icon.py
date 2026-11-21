"""Generate assets/app.ico: a yellow sun/brightness glyph, no external deps."""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "assets", "app.ico")
COLOR = (255, 205, 70)


def pixels(size):
    scale = 3
    n = size * scale
    cx = cy = (n - 1) / 2.0
    core_r = n * 0.185
    ray_in, ray_out = n * 0.29, n * 0.45
    grid = [[COLOR + (0,) for _ in range(n)] for _ in range(n)]
    for y in range(n):
        for x in range(n):
            dx, dy = x - cx, y - cy
            d = math.hypot(dx, dy)
            angle = math.atan2(dy, dx)
            core = d <= core_r
            ray = ray_in <= d <= ray_out and abs(math.cos(angle * 4.0)) > 0.93
            if core or ray:
                grid[y][x] = COLOR + (255,)
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
