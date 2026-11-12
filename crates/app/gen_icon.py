"""Generate crates/app/icons/icon.ico with only the stdlib.

One-shot build tool: draws a simple app mark (dark rounded square, orange
speech-bubble dot pair) and packs it as a PNG-in-ICO (Vista+ format).
Run: python gen_icon.py
"""
import struct
import zlib
from pathlib import Path

SIZE = 256
BG = (30, 34, 41, 255)        # dark panel
ACCENT = (232, 146, 107, 255)  # warm orange (matches the UI accent)
RADIUS = 56

def in_rounded(x: int, y: int) -> bool:
    """Inside the rounded-square canvas shape."""
    r = RADIUS
    cx = min(max(x, r), SIZE - 1 - r)
    cy = min(max(y, r), SIZE - 1 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r

def in_bubble(x: int, y: int) -> bool:
    """Chat bubble: big circle + tail wedge, centered slightly high."""
    dx, dy = x - 128, y - 116
    if dx * dx + dy * dy <= 66 * 66:
        return True
    # tail: triangle-ish region below-left of the circle
    return 40 <= x <= 84 and 160 <= y <= 176 and (y - 160) <= (84 - x) * 0.4

def pixels() -> bytes:
    rows = bytearray()
    for y in range(SIZE):
        rows.append(0)  # PNG filter: none
        for x in range(SIZE):
            if not in_rounded(x, y):
                rows += bytes((0, 0, 0, 0))
            elif in_bubble(x, y):
                rows += bytes(ACCENT)
            else:
                rows += bytes(BG)
    return bytes(rows)

def png(raw: bytes) -> bytes:
    def chunk(tag: bytes, data: bytes) -> bytes:
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c))
    ihdr = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", ihdr)
            + chunk(b"IDAT", zlib.compress(raw, 9))
            + chunk(b"IEND", b""))

def main() -> None:
    raw = pixels()
    body = png(raw)
    # ICONDIR + one ICONDIRENTRY (width/height 0 == 256) + PNG data
    ico = (struct.pack("<HHH", 0, 1, 1)
           + struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(body), 22)
           + body)
    out = Path(__file__).parent / "icons" / "icon.ico"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_bytes(ico)
    print(f"wrote {out} ({len(ico)} bytes)")

if __name__ == "__main__":
    main()
