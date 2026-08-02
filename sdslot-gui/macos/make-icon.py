#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Regenerate ../assets/sdslot.icns — an SD card in the log pane's phosphor
green. The .icns is checked in, so bundle.sh needs no Python; run this only
when the artwork changes.

Requires Pillow and macOS `iconutil`.
"""

import pathlib
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw

ASSETS = pathlib.Path(__file__).resolve().parent.parent / "assets"

# theme.rs: PHOSPHOR_GREEN / PHOSPHOR_BG.
GREEN = (0x3D, 0xFF, 0x66, 0xFF)
GREEN_DIM = (0x1E, 0x80, 0x33, 0xFF)
BODY = (0x1B, 0x22, 0x2B, 0xFF)
BODY_EDGE = (0x39, 0x44, 0x52, 0xFF)
BG = (0x06, 0x0E, 0x08, 0xFF)


def render(px: int) -> Image.Image:
    """Draw at 8x and downsample: cheap antialiasing without a vector stack."""
    s = px * 8
    img = Image.new("RGBA", (s, s), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)

    u = s / 100.0  # 1 unit == 1% of the canvas

    # Rounded-square backdrop, in the CRT's near-black.
    d.rounded_rectangle([2 * u, 2 * u, 98 * u, 98 * u], radius=22 * u, fill=BG)

    # SD card silhouette: rounded body with the top-left corner cut off.
    left, right, top, bottom = 24 * u, 76 * u, 14 * u, 86 * u
    cut = 15 * u
    d.rounded_rectangle([left, top, right, bottom], radius=6 * u, fill=BODY,
                        outline=BODY_EDGE, width=max(1, int(1.5 * u)))
    # Re-cut the corner back to the backdrop, then repaint the diagonal.
    d.polygon([(left - u, top - u), (left + cut, top - u), (left - u, top + cut)],
              fill=BG)
    d.polygon([(left, top + cut), (left + cut, top), (left + cut, top + 2 * u),
               (left + 2 * u, top + cut)], fill=BODY_EDGE)

    # Contact fingers along the top edge, clear of the cut corner.
    fx, fw, gap = left + cut + 4 * u, 5 * u, 3 * u
    while fx + fw <= right - 4 * u:
        d.rounded_rectangle([fx, top + 6 * u, fx + fw, top + 24 * u],
                            radius=1.5 * u, fill=GREEN_DIM)
        fx += fw + gap

    # Slot bands: what sdslot actually writes to the card.
    for i, y in enumerate((40, 54, 68)):
        w = (right - left) - 16 * u
        d.rounded_rectangle([left + 8 * u, y * u, left + 8 * u + w * (1.0 - 0.18 * i),
                             (y + 8) * u], radius=2 * u, fill=GREEN)

    return img.resize((px, px), Image.LANCZOS)


def main() -> int:
    if not shutil.which("iconutil"):
        print("iconutil not found (macOS only)", file=sys.stderr)
        return 1
    ASSETS.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "sdslot.iconset"
        iconset.mkdir()
        for base in (16, 32, 128, 256, 512):
            render(base).save(iconset / f"icon_{base}x{base}.png")
            render(base * 2).save(iconset / f"icon_{base}x{base}@2x.png")
        out = ASSETS / "sdslot.icns"
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)],
                       check=True)
        print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
