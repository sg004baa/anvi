#!/usr/bin/env python3
"""assets/anvi.ico を生成する。

アイコンの絵はここが唯一の出典。exe に埋め込まれた ICO をトレイも使う
（`tray.rs` は `Icon::from_resource(1, ...)` で読むだけ）ので、絵を変えるなら
このスクリプトを直して再生成し、生成物をコミットすること。

    scripts/make-icon.py [出力先]

角丸の四角（背景）に 5x7 ビットマップの "A" を載せる。背景の角だけ 4x
スーパーサンプリングし、グリフは整数倍拡大のままにしてドットを潰さない。
"""

import struct
import sys
import zlib
from pathlib import Path

BG = (0x24, 0x2B, 0x33, 0xFF)
FG = (0x8E, 0xC0, 0x7C, 0xFF)
GLYPH = [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001]
GLYPH_W = 5
GLYPH_H = 7
# 32px のとき半径 6 / グリフ 4 倍（= トレイの見た目）。他のサイズはこの比で伸ばす。
RADIUS_RATIO = 6 / 32
GLYPH_RATIO = 4 / 32
SIZES = [16, 20, 24, 32, 48, 64, 128, 256]
# 128 以上は PNG で持つ。BMP で持つと 256x256 だけで 256KB を超える。
PNG_FROM = 128
SUPERSAMPLE = 4


def coverage(size: int, x: int, y: int) -> float:
    """角丸の内側に入っている面積の割合（0.0〜1.0）。"""
    radius = size * RADIUS_RATIO
    hits = 0
    for sy in range(SUPERSAMPLE):
        for sx in range(SUPERSAMPLE):
            px = x + (sx + 0.5) / SUPERSAMPLE
            py = y + (sy + 0.5) / SUPERSAMPLE
            cx = radius if px < radius else (size - radius if px > size - radius else None)
            cy = radius if py < radius else (size - radius if py > size - radius else None)
            if cx is None or cy is None:
                hits += 1
                continue
            if (px - cx) ** 2 + (py - cy) ** 2 <= radius * radius:
                hits += 1
    return hits / (SUPERSAMPLE * SUPERSAMPLE)


def render(size: int) -> list[list[tuple[int, int, int, int]]]:
    px = [[(0, 0, 0, 0)] * size for _ in range(size)]
    for y in range(size):
        for x in range(size):
            a = coverage(size, x, y)
            if a > 0:
                px[y][x] = (BG[0], BG[1], BG[2], round(BG[3] * a))

    scale = max(1, round(size * GLYPH_RATIO))
    ox = (size - GLYPH_W * scale) // 2
    oy = (size - GLYPH_H * scale) // 2
    for row, bits in enumerate(GLYPH):
        for col in range(GLYPH_W):
            if not bits & (1 << (GLYPH_W - 1 - col)):
                continue
            for dy in range(scale):
                for dx in range(scale):
                    x = ox + col * scale + dx
                    y = oy + row * scale + dy
                    if 0 <= x < size and 0 <= y < size:
                        px[y][x] = FG
    return px


def as_bmp(px: list[list[tuple[int, int, int, int]]]) -> bytes:
    """ICO 内の BMP（BITMAPINFOHEADER + BGRA + AND マスク）。高さは 2 倍で書く。"""
    size = len(px)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    body = bytearray()
    for y in reversed(range(size)):
        for r, g, b, a in px[y]:
            body += bytes((b, g, r, a))
    # 32bpp でも AND マスクは要る。全画素「不透明」= 0 で埋め、行は 4 バイト境界。
    stride = ((size + 31) // 32) * 4
    body += bytes(stride * size)
    return header + bytes(body)


def as_png(px: list[list[tuple[int, int, int, int]]]) -> bytes:
    size = len(px)
    raw = bytearray()
    for row in px:
        raw.append(0)
        for r, g, b, a in row:
            raw += bytes((r, g, b, a))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    dest = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "assets" / "anvi.ico"
    images = []
    for size in SIZES:
        px = render(size)
        images.append((size, as_png(px) if size >= PNG_FROM else as_bmp(px)))

    offset = 6 + 16 * len(images)
    entries = bytearray()
    payload = bytearray()
    for size, data in images:
        entries += struct.pack(
            "<BBBBHHII", size & 0xFF, size & 0xFF, 0, 0, 1, 32, len(data), offset
        )
        payload += data
        offset += len(data)

    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_bytes(struct.pack("<HHH", 0, 1, len(images)) + bytes(entries) + bytes(payload))
    print(f"wrote {dest} ({dest.stat().st_size} bytes, sizes {SIZES})")


if __name__ == "__main__":
    main()
