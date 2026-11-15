"""Regenerate assets: uv run --no-project --with pillow python scripts/generate-icon.py"""

from html import escape
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ASSETS = Path(__file__).resolve().parents[1] / "crates" / "desktop" / "assets"
SCALE = 4
SIZE = 256
INK = "#38364F"
SUBJECT_SCALE = 1.45


def render_icon():
    """The selected large pocket computer on a warm orange tile."""
    image = Image.new("RGBA", (SIZE * SCALE, SIZE * SCALE))
    draw = ImageDraw.Draw(image)
    vector = []
    subject_scale = 1.0

    def scaled(value):
        return 252 + (value - 252) * subject_scale

    def ellipse(bounds, fill):
        bounds = tuple(scaled(value) for value in bounds)
        x0, y0, x1, y1 = bounds
        draw.ellipse(tuple(round(n * SCALE) for n in bounds), fill=fill)
        vector.append(
            f'<ellipse cx="{(x0 + x1) / 2:g}" cy="{(y0 + y1) / 2:g}" '
            f'rx="{(x1 - x0) / 2:g}" ry="{(y1 - y0) / 2:g}" fill="{fill}"/>'
        )

    def path(commands, fill=None, stroke=None, width=1):
        commands = [(op, *[scaled(n) for n in values]) for op, *values in commands]
        width *= subject_scale
        points = []
        svg = []
        current = (0, 0)
        for op, *values in commands:
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
                        u**3 * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t**3 * x3,
                        u**3 * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t**3 * y3,
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
            f'stroke="{stroke or "none"}" stroke-width="{width:g}" '
            'stroke-linecap="round" stroke-linejoin="round"/>'
        )

    draw.rounded_rectangle((4 * SCALE, 4 * SCALE, 252 * SCALE, 252 * SCALE),
                           radius=56 * SCALE, fill="#EDBA91")
    vector.append('<rect x="4" y="4" width="248" height="248" rx="56" fill="#EDBA91"/>')
    path([
        ("M", 112, 252), ("C", 109, 204, 125, 158, 171, 139),
        ("C", 200, 126, 231, 128, 252, 144), ("L", 252, 252), ("Z",),
    ], fill="#D79974")
    path([
        ("M", 59, 41.8), ("C", 61.8, 57.2, 68.8, 64.2, 84.2, 67),
        ("C", 68.8, 69.8, 61.8, 76.8, 59, 92.2),
        ("C", 56.2, 76.8, 49.2, 69.8, 33.8, 67),
        ("C", 49.2, 64.2, 56.2, 57.2, 59, 41.8), ("Z",),
    ], fill="#FFF9F4")

    # Scale only the computer, anchored at the corner, to match the selected preview.
    subject_scale = SUBJECT_SCALE
    body = [
        ("M", 139, 138), ("C", 140, 126, 151, 120, 164, 122),
        ("L", 261, 136), ("C", 275, 138, 281, 150, 279, 164),
        ("L", 266, 267), ("L", 126, 265), ("Z",),
    ]
    path(body, fill="#E9F5E4", stroke="#FFFDFC", width=10.5)
    path(body, fill="#E9F5E4", stroke=INK, width=4.5)
    path([
        ("M", 155, 145), ("L", 257, 158), ("L", 249, 224),
        ("L", 146, 211), ("Z",),
    ], fill="#47615F", stroke=INK, width=3)
    path([
        ("M", 161, 156), ("L", 169, 162), ("L", 159, 167),
    ], stroke="#C8EBD4", width=3.6)
    ellipse((178, 175, 189, 192), "#DDF7D7")
    ellipse((225, 181, 236, 198), "#DDF7D7")
    path([
        ("M", 198, 199), ("C", 202, 206, 210, 206, 215, 197),
    ], stroke="#DDF7D7", width=3.6)
    ellipse((235, 236, 244, 245), "#EAA588")
    path([("M", 147, 233), ("L", 172, 236)], stroke="#A0B7A5", width=4)
    path([("M", 143, 244), ("L", 163, 246)], stroke="#A0B7A5", width=3)

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
    print(f"Generated selected pocket-computer PNG, ICO and SVG in {ASSETS}")
