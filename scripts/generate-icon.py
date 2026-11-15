"""Regenerate assets: uv run --no-project --with pillow python scripts/generate-icon.py"""

from html import escape
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ASSETS = Path(__file__).resolve().parents[1] / "crates" / "desktop" / "assets"
SCALE = 4
SIZE = 256
INK = "#383550"
CREAM = "#FFF6ED"


def render_icon():
    """An original cartoon kitten peeking into the tile from the lower right."""
    image = Image.new("RGBA", (SIZE * SCALE, SIZE * SCALE))
    draw = ImageDraw.Draw(image)
    vector = []

    def ellipse(bounds, fill, stroke=None, width=1):
        x0, y0, x1, y1 = bounds
        draw.ellipse(
            tuple(round(n * SCALE) for n in bounds),
            fill=fill,
            outline=stroke,
            width=round(width * SCALE),
        )
        vector.append(
            f'<ellipse cx="{(x0 + x1) / 2:g}" cy="{(y0 + y1) / 2:g}" '
            f'rx="{(x1 - x0) / 2:g}" ry="{(y1 - y0) / 2:g}" '
            f'fill="{fill}" stroke="{stroke or "none"}" stroke-width="{width}"/>'
        )

    def path(commands, fill=None, stroke=None, width=1):
        points = []
        svg = []
        current = (0, 0)
        for command in commands:
            op, *values = command
            svg.append(op + " ".join(f"{n:g}" for n in values))
            if op in ("M", "L"):
                current = tuple(values)
                points.append(current)
            elif op == "C":
                x0, y0 = current
                x1, y1, x2, y2, x3, y3 = values
                for step in range(1, 33):
                    t = step / 32
                    u = 1 - t
                    points.append((
                        u**3 * x0 + 3 * u**2 * t * x1 + 3 * u * t**2 * x2 + t**3 * x3,
                        u**3 * y0 + 3 * u**2 * t * y1 + 3 * u * t**2 * y2 + t**3 * y3,
                    ))
                current = (x3, y3)
            elif op == "Z":
                points.append(points[0])
        pixels = [(round(x * SCALE), round(y * SCALE)) for x, y in points]
        if fill:
            draw.polygon(pixels, fill=fill)
        if stroke:
            draw.line(pixels, fill=stroke, width=round(width * SCALE), joint="curve")
            if commands[-1][0] != "Z":
                radius = width * SCALE / 2
                for x, y in (pixels[0], pixels[-1]):
                    draw.ellipse((x - radius, y - radius, x + radius, y + radius), fill=stroke)
        vector.append(
            f'<path d="{escape(" ".join(svg))}" fill="{fill or "none"}" '
            f'stroke="{stroke or "none"}" stroke-width="{width}" '
            'stroke-linecap="round" stroke-linejoin="round"/>'
        )

    draw.rounded_rectangle((4 * SCALE, 4 * SCALE, 252 * SCALE, 252 * SCALE),
                           radius=56 * SCALE, fill="#BCB6EF")
    vector.append('<rect x="4" y="4" width="248" height="248" rx="56" fill="#BCB6EF"/>')
    # A quiet opening at the corner gives the head somewhere to peek from.
    path([
        ("M", 113, 252), ("C", 107, 210, 117, 160, 157, 139),
        ("C", 188, 122, 228, 124, 252, 143), ("L", 252, 252), ("Z",),
    ], fill="#9C94D4")
    path([
        ("M", 58, 47), ("C", 60, 59, 65, 64, 77, 66),
        ("C", 65, 68, 60, 73, 58, 85), ("C", 56, 73, 51, 68, 39, 66),
        ("C", 51, 64, 56, 59, 58, 47), ("Z",),
    ], fill="#FAF5FF")

    # Round cheeks, oversized animated eyes and soft ears identify a creature.
    # The right cheek and chin continue out of frame to create the peeking pose.
    face = [
        ("M", 139, 166), ("C", 131, 149, 128, 127, 133, 108),
        ("C", 147, 111, 162, 124, 171, 138),
        ("C", 191, 126, 218, 125, 238, 136),
        ("C", 245, 121, 255, 109, 266, 108),
        ("C", 270, 126, 270, 145, 265, 164),
        ("C", 281, 184, 281, 212, 268, 234),
        ("C", 258, 259, 233, 274, 204, 276),
        ("C", 171, 277, 143, 258, 134, 237),
        ("C", 123, 220, 123, 190, 139, 166), ("Z",),
    ]
    path(face, fill=CREAM, stroke="#FFFDFC", width=11)
    path(face, fill=CREAM, stroke=INK, width=4.5)
    path([
        ("M", 140, 124), ("C", 140, 135, 145, 146, 151, 151),
        ("L", 159, 144), ("C", 153, 134, 147, 127, 140, 124), ("Z",),
    ], fill="#E5B3CC")
    path([
        ("M", 258, 125), ("C", 251, 131, 247, 141, 246, 146),
        ("L", 259, 154), ("C", 262, 143, 261, 134, 258, 125), ("Z",),
    ], fill="#E5B3CC")
    path([
        ("M", 178, 145), ("C", 188, 138, 206, 135, 217, 140),
        ("C", 210, 152, 193, 158, 180, 153), ("Z",),
    ], fill="#AED6C8")

    ellipse((159, 181, 183, 212), INK)
    ellipse((219, 176, 243, 207), INK)
    ellipse((164, 183, 173, 193), "#FFFDFC")
    ellipse((224, 178, 233, 188), "#FFFDFC")
    ellipse((173, 201, 178, 206), "#A69CD1")
    ellipse((233, 196, 238, 201), "#A69CD1")
    ellipse((147, 216, 173, 229), "#F3B2BE")
    ellipse((237, 210, 263, 223), "#F3B2BE")
    path([
        ("M", 194, 214), ("C", 196, 212, 201, 212, 203, 214),
        ("L", 199, 218), ("Z",),
    ], fill="#C58CA1")
    path([
        ("M", 188, 222), ("C", 189, 230, 196, 231, 199, 224),
        ("C", 203, 231, 211, 228, 212, 220),
    ], stroke=INK, width=4)
    # A small padded paw hooks over the opening.
    paw = [
        ("M", 115, 232), ("C", 110, 225, 111, 217, 118, 215),
        ("C", 120, 206, 129, 205, 133, 211),
        ("C", 139, 208, 146, 212, 146, 219),
        ("C", 153, 223, 150, 235, 144, 238),
        ("C", 133, 243, 121, 241, 115, 232), ("Z",),
    ]
    path(paw, fill=CREAM, stroke="#FFFDFC", width=9)
    path(paw, fill=CREAM, stroke=INK, width=3.5)
    ellipse((123, 224, 139, 235), "#E5B3CC")
    ellipse((120, 215, 125, 220), "#E5B3CC")
    ellipse((130, 214, 135, 219), "#E5B3CC")
    ellipse((139, 219, 144, 224), "#E5B3CC")

    mask = Image.new("L", image.size)
    ImageDraw.Draw(mask).rounded_rectangle(
        (4 * SCALE, 4 * SCALE, 252 * SCALE, 252 * SCALE), radius=56 * SCALE, fill=255)
    image.putalpha(ImageChops.multiply(image.getchannel("A"), mask))
    svg = (
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">\n'
        '<defs><clipPath id="tile"><rect x="4" y="4" width="248" height="248" rx="56"/></clipPath></defs>\n'
        '<g clip-path="url(#tile)">\n' + "\n".join(vector) + '\n</g>\n</svg>\n'
    )
    return image, svg


if __name__ == "__main__":
    ASSETS.mkdir(parents=True, exist_ok=True)
    image, svg = render_icon()
    image.resize((256, 256), Image.Resampling.LANCZOS).save(ASSETS / "pi-qq.png")
    sizes = (16, 20, 24, 32, 40, 48, 64, 128, 256)
    image.save(ASSETS / "pi-qq.ico", sizes=[(n, n) for n in sizes])
    (ASSETS / "pi-qq.svg").write_text(svg, encoding="utf-8")
    print(f"Generated corner-peeking cartoon kitten PNG, ICO and SVG in {ASSETS}")
