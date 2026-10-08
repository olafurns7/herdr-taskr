#!/usr/bin/env python3
"""Paint the frames as PNGs and one contact sheet. Usage: png.py DIR

Reads DIR/*.ansi as written by `cargo run -p taskr-tui --example frames -- DIR` (one
truecolor SGR per cell) and writes DIR/NAME.png and DIR/sheet.png. Needs Pillow and
DejaVu Sans Mono, so glyph shapes and line gaps differ from the owner's terminal font;
`cat DIR/NAME.ansi` in a pane of the right size shows the real thing.
"""
import glob, os, re, sys
from PIL import Image, ImageDraw, ImageFont

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono"
font, bold = ImageFont.truetype(FONT + ".ttf", 15), ImageFont.truetype(FONT + "-Bold.ttf", 15)
CW, CH, PAD = 9, 19, 8
# The terminal behind the UI: Catppuccin Mocha and Latte base and text.
DARK, LIGHT = ((0x1E, 0x1E, 0x2E), (0xCD, 0xD6, 0xF4)), ((0xEF, 0xF1, 0xF5), (0x4C, 0x4F, 0x69))
CELL = re.compile(r"\x1b\[0;([0-9;]*)m([^\x1b\n]*)")


def paint(path):
    back, fore = LIGHT if path.endswith("-light.ansi") else DARK
    rows = [CELL.findall(line) for line in open(path, encoding="utf-8").read().split("\n") if line]
    image = Image.new("RGB", (max(len(r) for r in rows) * CW + 2 * PAD, len(rows) * CH + 2 * PAD), back)
    draw = ImageDraw.Draw(image)
    for y, row in enumerate(rows):
        for x, (sgr, text) in enumerate(row):
            codes = [int(c) for c in sgr.split(";")]
            fg, bg, heavy = fore, None, codes[-1] == 1 and codes[-4:-3] != [2]
            for i, code in enumerate(codes):
                if code in (38, 48) and codes[i + 1 : i + 2] == [2]:
                    color = tuple(codes[i + 2 : i + 5])
                    fg, bg = (color, bg) if code == 38 else (fg, color)
            left, top = PAD + x * CW, PAD + y * CH
            if bg:
                draw.rectangle([left, top, left + CW - 1, top + CH - 1], fill=bg)
            if text.strip():
                draw.text((left, top + 1), text, font=bold if heavy else font, fill=fg)
    image.save(path[:-5] + ".png")
    return os.path.basename(path)[:-5], image


def sheet(frames, out, width=3400, gap=24):
    """Shelf packing: narrow frames first, a name over each."""
    frames.sort(key=lambda f: (f[1].width, f[0]))
    x = y = gap
    shelf, spots = 0, []
    for name, image in frames:
        if x + image.width + gap > width:
            x, y, shelf = gap, y + shelf + gap, 0
        spots.append((name, image, x, y))
        x, shelf = x + image.width + gap, max(shelf, image.height + CH + 4)
    page = Image.new("RGB", (width, y + shelf + gap), (0x11, 0x11, 0x1B))
    draw = ImageDraw.Draw(page)
    for name, image, x, y in spots:
        draw.text((x, y), name, font=bold, fill=DARK[1])
        page.paste(image, (x, y + CH + 4))
    page.save(out)


if __name__ == "__main__":
    folder = sys.argv[1]
    sheet([paint(p) for p in sorted(glob.glob(os.path.join(folder, "*.ansi")))], os.path.join(folder, "sheet.png"))
