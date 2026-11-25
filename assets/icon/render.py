"""Renders the BatterySight logo SVGs into batterysight.ico (the exe icon) and logo.png.

Needs resvg (https://github.com/linebender/resvg/releases; on PATH, or its path in the
RESVG environment variable) and Pillow (`pip install pillow`).
Run from anywhere: `python assets/icon/render.py`.
"""

import io
import os
import subprocess
import tempfile
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
RESVG = os.environ.get("RESVG", "resvg")

# Each icon size and the drawing it is rendered from: the small sizes have their own
# drawings fitted to the pixel grid, the rest scale the detailed master.
SOURCES = {
    16: "batterysight-16.svg",
    20: "batterysight-20.svg",
    24: "batterysight-24.svg",
    32: "batterysight-32.svg",
    40: "batterysight.svg",
    48: "batterysight.svg",
    64: "batterysight.svg",
    256: "batterysight.svg",
}


def render(svg: str, size: int) -> Image.Image:
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "out.png"
        subprocess.run(
            [RESVG, "-w", str(size), "-h", str(size), str(HERE / svg), str(out)], check=True
        )
        return Image.open(io.BytesIO(out.read_bytes())).convert("RGBA")


def main() -> None:
    frames = [render(svg, size) for size, svg in sorted(SOURCES.items())]
    frames[-1].save(
        HERE / "batterysight.ico",
        format="ICO",
        sizes=[f.size for f in frames],
        append_images=frames[:-1],
    )
    render("batterysight.svg", 256).save(HERE / "logo.png", optimize=True)


if __name__ == "__main__":
    main()
