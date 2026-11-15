"""Regenerate assets: uv run --no-project --with pillow python scripts/generate-icon.py"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

assets = Path(__file__).resolve().parents[1] / "crates" / "desktop" / "assets"
assets.mkdir(parents=True, exist_ok=True)
# Original 32 px penguin with a chat-bubble belly; no brand lettering or scarf.
mask = Image.new("L", (32, 32))
draw = ImageDraw.Draw(mask)
draw.polygon([
    (10, 3), (21, 3), (21, 5), (23, 5), (23, 12), (22, 12),
    (22, 14), (24, 14), (24, 16), (26, 16), (26, 20), (28, 20),
    (28, 22), (25, 22), (25, 20), (24, 20), (24, 25), (22, 25),
    (22, 27), (24, 27), (24, 29), (18, 29), (18, 27), (13, 27),
    (13, 29), (7, 29), (7, 27), (9, 27), (9, 25), (7, 25),
    (7, 20), (6, 20), (6, 22), (3, 22), (3, 20), (5, 20),
    (5, 16), (7, 16), (7, 14), (9, 14), (9, 12), (8, 12),
    (8, 5), (10, 5),
], fill=255)
# A one-pixel white keyline keeps the silhouette visible on dark taskbars.
icon = Image.new("RGBA", (32, 32))
icon.paste("white", (0, 0, 32, 32), mask.filter(ImageFilter.MaxFilter(3)))
icon.paste("black", (0, 0, 32, 32), mask)
draw = ImageDraw.Draw(icon)
draw.rectangle((11, 7, 13, 9), fill="white")
draw.rectangle((18, 7, 20, 9), fill="white")
draw.point((12, 8), fill="black")
draw.point((19, 8), fill="black")
draw.rectangle((14, 11, 17, 11), fill="white")
draw.rectangle((15, 12, 16, 12), fill="white")
draw.polygon([(11, 15), (20, 15), (20, 16), (22, 16), (22, 23),
              (20, 23), (20, 24), (15, 24), (15, 26), (12, 26),
              (12, 24), (9, 24), (9, 17), (11, 17)], fill="white")
draw.rectangle((12, 19, 13, 20), fill="black")
draw.rectangle((17, 19, 18, 20), fill="black")
large = icon.resize((256, 256), Image.Resampling.NEAREST)
large.save(assets / "pi-qq.png")
sizes = (16, 20, 24, 32, 40, 48, 64, 128, 256)
large.save(assets / "pi-qq.ico", sizes=[(n, n) for n in sizes],
           append_images=[icon.resize((n, n), Image.Resampling.NEAREST) for n in sizes[:-1]])
# Keep an editable vector source with exact pixel-aligned cells.
rectangles = []
for y in range(32):
    x = 0
    while x < 32:
        color = icon.getpixel((x, y))
        end = x + 1
        while end < 32 and icon.getpixel((end, y)) == color:
            end += 1
        if color[3]:
            fill = "#fff" if color[0] else "#000"
            rectangles.append(f'<rect x="{x}" y="{y}" width="{end-x}" height="1" fill="{fill}"/>')
        x = end
(assets / "pi-qq.svg").write_text(
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" shape-rendering="crispEdges">\n'
    + "\n".join(rectangles) + "\n</svg>\n", encoding="utf-8")
print(f"Generated monochrome pixel PNG, ICO and SVG in {assets}")
