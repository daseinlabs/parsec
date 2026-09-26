"""Export the Parsec logo suite from its supplied vector artwork.

Requires PyMuPDF for PNG rendering. Run from any directory. The full logo.svg is
the unchanged supplied master; parsec-symbol.svg holds the standalone PDF mark.
No text is re-typeset and no symbol is redrawn. Legacy glow filenames are flat
compatibility exports. All PNG dimensions are explicit and reproducible.
"""

from copy import deepcopy
import json
from pathlib import Path
import xml.etree.ElementTree as ET

import pymupdf

HERE = Path(__file__).resolve().parent
SVG = HERE / "svg"
PNG = HERE / "png"
NS = "http://www.w3.org/2000/svg"
ET.register_namespace("", NS)
TOKENS = json.loads((HERE.parent / "tokens/tokens.json").read_text())
BLACK = TOKENS["color"]["brand"]["black"]["value"]


def serialize(root):
    return ET.tostring(root, encoding="unicode") + "\n"


def recolor(root, color):
    root = deepcopy(root)
    for node in root.iter():
        if node.get("fill") and node.get("fill") != "none":
            node.set("fill", color)
    return root


def tile(symbol, size, shape="rounded"):
    root = ET.Element(f"{{{NS}}}svg", {
        "width": str(size), "height": str(size), "viewBox": "0 0 512 512",
        "role": "img", "aria-label": "Parsec icon",
    })
    ET.SubElement(root, f"{{{NS}}}title").text = "Parsec icon"
    if shape == "circle":
        ET.SubElement(root, f"{{{NS}}}circle", {"cx": "256", "cy": "256", "r": "256", "fill": BLACK})
    else:
        ET.SubElement(root, f"{{{NS}}}rect", {
            "width": "512", "height": "512", "rx": "36" if shape == "rounded" else "0", "fill": BLACK,
        })
    # Leave a small margin around the complete horizontal line; never stretch it.
    width = 480
    height = width * 337 / 657
    # A group transform also works in renderers that ignore nested SVG x/y.
    x, y, view_width, _ = map(float, symbol.get("viewBox").split())
    group = ET.SubElement(root, f"{{{NS}}}g", {
        "transform": f"translate({(512 - width) / 2} {(512 - height) / 2}) "
                     f"scale({width / view_width}) translate({-x} {-y})",
    })
    for node in symbol:
        if node.tag not in (f"{{{NS}}}title", f"{{{NS}}}desc"):
            group.append(deepcopy(node))
    return root


def raster(root, filename, width=None, background=False):
    root = deepcopy(root)
    if width is not None:
        ratio = float(root.get("height")) / float(root.get("width"))
        root.set("width", str(width))
        root.set("height", str(round(width * ratio)))
    if background:
        x, y, w, h = root.get("viewBox").split()
        root.insert(0, ET.Element(f"{{{NS}}}rect", {"x": x, "y": y, "width": w, "height": h, "fill": BLACK}))
    with pymupdf.open(stream=serialize(root).encode(), filetype="svg") as doc:
        # SVG dimensions are CSS pixels; rendering at 72 dpi produces those sizes.
        doc[0].get_pixmap(alpha=True).save(PNG / filename)


def generate():
    PNG.mkdir(exist_ok=True)
    logo = ET.parse(SVG / "logo.svg").getroot()
    symbol = ET.parse(SVG / "parsec-symbol.svg").getroot()
    black_logo = recolor(logo, BLACK)
    black_symbol = recolor(symbol, BLACK)
    exports = {
        "parsec-logo-black.svg": black_logo,
        "parsec-symbol-black.svg": black_symbol,
        "parsec-mark-flat.svg": symbol,
        "parsec-mark-glow.svg": symbol,
        "parsec-icon-512.svg": tile(symbol, 512),
        "parsec-avatar.svg": tile(symbol, 512, "circle"),
        **{f"parsec-favicon-{s}.svg": tile(symbol, s, "square") for s in (16, 32, 64)},
    }
    for name, root in exports.items():
        (SVG / name).write_text(serialize(root))
    raster(logo, "parsec-logo.png")
    raster(black_logo, "parsec-logo-black.png")
    raster(symbol, "parsec-mark-flat.png", width=1314)
    raster(symbol, "parsec-mark-glow.png", width=1314)
    raster(symbol, "parsec-mark-glow-dark.png", width=1314, background=True)
    raster(black_symbol, "parsec-symbol-black.png", width=1314)
    for size in (512, 1024):
        raster(tile(symbol, size), f"parsec-icon-{size}.png")
    raster(tile(symbol, 512, "circle"), "parsec-avatar.png")
    for size in (16, 32, 64):
        raster(tile(symbol, size, "square"), f"parsec-favicon-{size}.png")


if __name__ == "__main__":
    generate()
    print("Generated the Parsec SVG and PNG logo suite")
