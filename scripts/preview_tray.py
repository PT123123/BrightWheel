"""Preview candidate tray icons at several brightness levels / sizes.

Renders with the exact same math that will be ported to Rust make_icon(),
so what you see here is what ships. Dark background matches Windows tray.
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "tray_preview.png")
SUN = (255, 205, 72)
SUN_LO = (232, 150, 46)
# No dark track ring: an unfilled grey circle reads as a black frame.
TRACK = (0, 0, 0)


def icon_pixels(n, percent):
    """n = logical size (e.g. 32). Returns n*n RGBA with 4x supersampling."""
    scale = 4
    cx = cy = (n - 1) / 2.0
    core_r = n * 0.175
    ray_in, ray_out = n * 0.205, n * 0.335
    ring_r = n * 0.415
    ring_hw = max(0.9, n * 0.035)
    t = max(0.0, min(1.0, percent / 100.0))
    sun = tuple(round(SUN_LO[i] + (SUN[i] - SUN_LO[i]) * t) for i in range(3))
    sweep = t * 360.0

    grid = [[(0, 0, 0, 0) for _ in range(n * scale)] for _ in range(n * scale)]
    for yy in range(n * scale):
        for xx in range(n * scale):
            x = xx + 0.5
            y = yy + 0.5
            dx = x - (cx + 0.5) * scale
            dy = y - (cy + 0.5) * scale
            d = math.hypot(dx, dy)
            # core disc
            if d <= core_r * scale:
                grid[yy][xx] = sun + (255,)
                continue
            # 8 tapered rays
            hit = False
            for k in range(8):
                a = k * math.pi / 4
                ux, uy = math.cos(a), math.sin(a)
                proj = dx * ux + dy * uy
                perp = -dx * uy + dy * ux
                if ray_in * scale <= proj <= ray_out * scale:
                    q = (proj - ray_in * scale) / (ray_out - ray_in) / scale
                    hw = (1.9 - 1.15 * q) * scale
                    if abs(perp) <= hw:
                        hit = True
                        break
            if hit:
                grid[yy][xx] = sun + (255,)
                continue
            # progress ring
            if abs(d - ring_r * scale) <= ring_hw * scale:
                deg = (math.degrees(math.atan2(dy, dx)) + 90.0 + 360.0) % 360.0
                if deg <= sweep:
                    grid[yy][xx] = sun + (255,)
                # else: leave transparent (no dark frame around the icon)

    out = []
    for y in range(n):
        row = []
        for x in range(n):
            acc = [0, 0, 0, 0]
            for sy in range(scale):
                for sx in range(scale):
                    p = grid[y * scale + sy][x * scale + sx]
                    for i in range(4):
                        acc[i] += p[i]
            row.append(tuple(v // (scale * scale) for v in acc))
        out.append(row)
    return out


def chunk(tag, data):
    return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)


def png_rgba(w, h, rows):
    raw = b"".join(b"\x00" + b"".join(bytes(p) for p in row) for row in rows)
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def main():
    # montage: rows = sizes (16,32,64), cols = brightness 0,25,50,75,100
    sizes = [16, 32, 64]
    levels = [0, 25, 50, 75, 100]
    cell = 96
    pad = 12
    W = pad * 2 + len(levels) * cell
    H = pad * 2 + len(sizes) * cell
    bg = (28, 30, 36, 255)
    canvas = [[bg for _ in range(W)] for _ in range(H)]
    for r, n in enumerate(sizes):
        for c, pct in enumerate(levels):
            px = icon_pixels(n, pct)
            ox = pad + c * cell + (cell - n) // 2
            oy = pad + r * cell + (cell - n) // 2
            for y in range(n):
                for x in range(n):
                    pr, pg, pb, pa = px[y][x]
                    a = pa / 255.0
                    dst = canvas[oy + y][ox + x]
                    canvas[oy + y][ox + x] = (
                        round(pr * a + dst[0] * (1 - a)),
                        round(pg * a + dst[1] * (1 - a)),
                        round(pb * a + dst[2] * (1 - a)),
                        255,
                    )
    with open(OUT, "wb") as f:
        f.write(png_rgba(W, H, canvas))
    print(f"wrote {OUT} ({W}x{H})")


if __name__ == "__main__":
    main()
