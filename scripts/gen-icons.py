#!/usr/bin/env python3
"""Regenerate every bundle icon from scratch.

Tauri needs a different container per platform:
  macOS   .icns  (multi-resolution)
  Windows .ico   (multi-resolution, PNG-compressed entries are fine on Vista+)
  Linux   .png   (a directory of sizes)

Shipping only the PNG set leaves Windows bundling with no icon and `cargo tauri
build --target x86_64-pc-windows-msvc` fails outright, so the .ico is generated
here alongside the rest.
"""

import os
import shutil
import struct
import subprocess
import tempfile
import zlib


def rgba_bubble(size):
    """Rounded chat bubble with three dots: legible down to 16 px."""
    width = height = size
    inset = size * 0.09
    radius = size * 0.26
    dot_r = size * 0.052
    x0, y0 = inset, inset
    x1, y1 = width - inset, height - inset

    def inside(x, y):
        if x < x0 or x > x1 or y < y0 or y > y1:
            return False
        cx = min(max(x, x0 + radius), x1 - radius)
        cy = min(max(y, y0 + radius), y1 - radius)
        return (x - cx) ** 2 + (y - cy) ** 2 <= radius * radius

    def dot(x, y):
        cy = height * 0.5
        for cx in (0.32, 0.5, 0.68):
            dx = x - width * cx
            if dx * dx + (y - cy) ** 2 <= dot_r * dot_r:
                return True
        return False

    rows = []
    for y in range(height):
        row = bytearray(b"\x00")
        for x in range(width):
            px, py = x + 0.5, y + 0.5
            if inside(px, py) and not dot(px, py):
                row += bytes((255, 255, 255, 255))
            else:
                row += bytes((0, 0, 0, 0))
        rows.append(bytes(row))
    return b"".join(rows)


def png_bytes(size):
    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    raw = rgba_bubble(size)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def ico_bytes(sizes):
    """ICONDIR + ICONDIRENTRY per size; entries hold PNG data (Vista+)."""
    images = [(s, png_bytes(s)) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    blobs = b""
    for size, data in images:
        dim = 0 if size >= 256 else size
        entries += struct.pack(
            "<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset
        )
        blobs += data
        offset += len(data)
    return header + entries + blobs


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    icons = os.path.join(here, "..", "src-tauri", "icons")
    os.makedirs(icons, exist_ok=True)

    png_sizes = [16, 32, 64, 128, 256, 512, 1024]
    for size in png_sizes:
        with open(os.path.join(icons, f"{size}x{size}.png"), "wb") as handle:
            handle.write(png_bytes(size))

    # icon.png is what Linux/AppImage and the tray fall back to.
    with open(os.path.join(icons, "icon.png"), "wb") as handle:
        handle.write(png_bytes(512))

    with open(os.path.join(icons, "icon.ico"), "wb") as handle:
        handle.write(ico_bytes([16, 32, 48, 64, 128, 256]))

    # .icns via iconutil so AppKit picks the right representation per context.
    if shutil.which("iconutil"):
        staging = tempfile.mkdtemp()
        iconset = os.path.join(staging, "icon.iconset")
        os.makedirs(iconset)
        mapping = {
            "icon_16x16.png": 16, "icon_16x16@2x.png": 32,
            "icon_32x32.png": 32, "icon_32x32@2x.png": 64,
            "icon_128x128.png": 128, "icon_128x128@2x.png": 256,
            "icon_256x256.png": 256, "icon_256x256@2x.png": 512,
            "icon_512x512.png": 512, "icon_512x512@2x.png": 1024,
        }
        for name, size in mapping.items():
            with open(os.path.join(iconset, name), "wb") as handle:
                handle.write(png_bytes(size))
        subprocess.run(
            ["iconutil", "-c", "icns", iconset, "-o", os.path.join(icons, "icon.icns")],
            check=True,
        )
        shutil.rmtree(staging)

    print("icons written:")
    for name in sorted(os.listdir(icons)):
        print(" ", name, os.path.getsize(os.path.join(icons, name)), "B")


if __name__ == "__main__":
    main()
