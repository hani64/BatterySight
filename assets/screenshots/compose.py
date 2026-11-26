"""Composes the README's screenshots and GitHub's social preview from the app's own 2x renders.

First render them (the app draws the widget, hover card and history chart itself):

    cargo test -p batterysight screenshots -- --ignored

then run `python assets/screenshots/compose.py` (needs Pillow and NumPy, and Windows for its
fonts). Around those renders it draws the parts Windows draws: the desktop and taskbar, the
menu and the history window's frame and controls, at 2x with Windows 11's fonts and metrics.
"""

from pathlib import Path

import numpy as np
from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parents[2]
RAW = ROOT / "target" / "screenshots"
OUT = Path(__file__).resolve().parent
LOGO = ROOT / "assets" / "icon" / "logo.png"
FONTS = Path("C:/Windows/Fonts")
X = 2  # everything is drawn at 2x: 1 unit below = 2 pixels


def font(name: str, px: float) -> ImageFont.FreeTypeFont:
    return ImageFont.truetype(str(FONTS / name), round(px * X))


UI = lambda px=12: font("segoeui.ttf", px)  # noqa: E731  Win32 menus and controls: 9 pt
CLOCK = lambda px=12: font("segoeui.ttf", px)  # noqa: E731
ICONS = lambda px=16: font("SegoeIcons.ttf", px)  # noqa: E731  Segoe Fluent Icons


def raw(name: str, opaque: bool = False) -> Image.Image:
    """A render from the screenshots test: premultiplied BGRA rows."""
    w, h = map(int, (RAW / f"{name}.txt").read_text().split())
    a = np.frombuffer((RAW / f"{name}.bgra").read_bytes(), np.uint8).reshape(h, w, 4)
    a = a.astype(np.float32)
    rgb = a[..., [2, 1, 0]]
    alpha = np.full((h, w), 255.0) if opaque else a[..., 3]
    if not opaque:
        nz = alpha > 0
        rgb[nz] = np.clip(rgb[nz] * 255 / alpha[nz, None], 0, 255)
    return Image.fromarray(np.dstack([rgb, alpha]).astype(np.uint8))


def rounded(im: Image.Image, radius: float) -> Image.Image:
    im = im.convert("RGBA")
    mask = Image.new("L", im.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, im.width - 1, im.height - 1), radius * X, fill=255)
    out = im.copy()
    out.putalpha(Image.fromarray(np.minimum(np.asarray(im.getchannel("A")), np.asarray(mask))))
    return out


def shadow(size, radius: float, blur: float, offset: float, opacity: int) -> Image.Image:
    """A soft shadow for a rounded rectangle of `size`, with room around it."""
    pad = round(blur * 3 * X)
    w, h = size[0] + 2 * pad, size[1] + 2 * pad
    mask = Image.new("L", (w, h), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (pad, pad + offset * X, pad + size[0], pad + size[1] + offset * X), radius * X, fill=opacity
    )
    out = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    out.putalpha(mask.filter(ImageFilter.GaussianBlur(blur * X)))
    return out, pad


def floating(im: Image.Image, radius: float, blur=16, offset=8, opacity=70) -> Image.Image:
    """`im` with rounded corners on a Windows 11 shadow, on a transparent canvas."""
    sh, pad = shadow(im.size, radius, blur, offset, opacity)
    sh.alpha_composite(rounded(im, radius), (pad, pad))
    return sh


def wallpaper(w: int, h: int, dark: bool) -> Image.Image:
    """An abstract Windows 11-like wallpaper: soft blue blooms."""
    yy, xx = np.mgrid[0:h, 0:w].astype(np.float32)
    base = np.array([10, 22, 48] if dark else [214, 228, 246], np.float32)
    img = np.ones((h, w, 3), np.float32) * base
    blooms = [  # (x, y, radius, colour) as fractions of the canvas
        (0.18, 0.95, 0.55, (0, 103, 192) if dark else (120, 175, 240)),
        (0.62, 0.65, 0.42, (46, 140, 255) if dark else (150, 200, 255)),
        (0.95, 0.15, 0.40, (0, 60, 140) if dark else (190, 215, 250)),
        (0.40, 0.10, 0.35, (20, 80, 170) if dark else (225, 236, 252)),
    ]
    for fx, fy, fr, col in blooms:
        d = np.hypot(xx - fx * w, yy - fy * h) / (fr * max(w, h))
        k = np.clip(1 - d, 0, 1) ** 2
        img = img * (1 - k[..., None] * 0.9) + np.array(col, np.float32) * k[..., None] * 0.9
    return Image.fromarray(np.clip(img, 0, 255).astype(np.uint8)).convert("RGBA")


def hero(dark: bool) -> Image.Image:
    """The taskbar's right end with the widget, the system tray and the clock, and the
    mouse's hover card above it, on a desktop."""
    theme = "dark" if dark else "light"
    widget, card = raw(f"widget-{theme}"), raw(f"card-{theme}")
    W, H, bar = 600 * X, 190 * X, 48 * X
    img = wallpaper(W, H, dark)
    # Taskbar: the desktop seen through a tinted, blurred surface (Mica), with a hairline.
    region = img.crop((0, H - bar, W, H)).filter(ImageFilter.GaussianBlur(30 * X))
    tint = Image.new("RGBA", region.size, (32, 32, 32, 235) if dark else (243, 243, 243, 235))
    region.alpha_composite(tint)
    img.paste(region, (0, H - bar))
    d = ImageDraw.Draw(img)
    hairline = Image.new("RGBA", (W, X), (255, 255, 255, 24) if dark else (0, 0, 0, 20))
    img.alpha_composite(hairline, (0, H - bar))
    fg = (255, 255, 255, 255) if dark else (26, 26, 26, 255)

    # From the right: clock, tray icons, the show-hidden chevron, then BatterySight.
    x = W - 16 * X
    for i, line in enumerate(("11:19 AM", "11/25/2026")):
        f = CLOCK(12)
        tw = d.textlength(line, font=f)
        d.text((x - tw, H - bar + (7 + i * 17) * X), line, font=f, fill=fg)
    x -= 84 * X
    for glyph in ("\uE995", "\uE701"):  # volume, Wi-Fi
        f = ICONS(16)
        tw = d.textlength(glyph, font=f)
        d.text((x - tw, H - bar + 16 * X), glyph, font=f, fill=fg)
        x -= 30 * X
    f = ICONS(12)
    d.text((x - d.textlength("\uE70E", font=f) - 6 * X, H - bar + 18 * X), "\uE70E", font=f, fill=fg)
    x -= 36 * X
    wx = x - widget.width
    img.alpha_composite(widget, (wx, H - bar))

    # The hover card over the mouse (the strip's middle device), 8 px above the taskbar.
    c = floating(card, 8, blur=12, offset=4, opacity=90 if dark else 60)
    cx = wx + widget.width // 2 - c.width // 2
    cy = H - bar - 8 * X - card.height - (c.height - card.height) // 2
    img.alpha_composite(c, (cx, cy))
    return rounded(img, 12)


MENU = [
    ("PRO X Wireless Gaming Headset \u2014 88%", "sub"),
    ("PRO X Superlight \u2014 70%", "sub"),
    ("Xbox Wireless Controller \u2014 45%", "sub"),
    None,
    ("Battery history\u2026", "hover"),
    ("Show on", "sub"),
    ("Show as", "sub"),
    ("Start with Windows", "check"),
    None,
    ("Refresh now", ""),
    ("Exit", ""),
]


def menu(dark: bool) -> Image.Image:
    """BatterySight's menu as Windows 11 draws a popup menu."""
    row, sep, pad, width = 30, 9, 4, 300
    h = pad * 2 + sum(row if item else sep for item in MENU)
    bg = (44, 44, 44) if dark else (249, 249, 249)
    fg = (255, 255, 255) if dark else (26, 26, 26)
    line = (64, 64, 64) if dark else (224, 224, 224)
    hover = (61, 61, 61) if dark else (236, 236, 236)
    border = (62, 62, 62) if dark else (222, 222, 222)
    img = Image.new("RGBA", (width * X, h * X), bg + (255,))
    d = ImageDraw.Draw(img)
    y = pad
    for item in MENU:
        if item is None:
            d.line((0, (y + sep / 2) * X, width * X, (y + sep / 2) * X), fill=line, width=X)
            y += sep
            continue
        text, kind = item
        if kind == "hover":
            d.rounded_rectangle((4 * X, (y + 1) * X, (width - 4) * X, (y + row - 1) * X), 4 * X, fill=hover)
        if kind == "check":
            d.text((14 * X, (y + 8) * X), "\uE73E", font=ICONS(12), fill=fg)
        d.text((40 * X, (y + 6) * X), text, font=UI(12), fill=fg)
        if kind == "sub":
            d.text(((width - 26) * X, (y + 9) * X), "\uE76C", font=ICONS(10), fill=fg)
        y += row
    d.rounded_rectangle((0, 0, img.width - 1, img.height - 1), 8 * X, outline=border, width=X)
    return floating(img, 8, blur=14, offset=6, opacity=110 if dark else 55)


def history() -> Image.Image:
    """The Battery history window: its frame and controls around the app's own chart."""
    chart = raw("chart", opaque=True)
    W, H, title = 952, 564, 32
    img = Image.new("RGBA", (W * X, H * X), (255, 255, 255, 255))
    d = ImageDraw.Draw(img)
    fg = (26, 26, 26)
    # Title bar: icon, title, caption buttons.
    d.rectangle((0, 0, W * X, title * X), fill=(243, 243, 243))
    logo = Image.open(LOGO).convert("RGBA").resize((16 * X, 16 * X), Image.LANCZOS)
    img.alpha_composite(logo, (10 * X, 8 * X))
    d.text((34 * X, 7 * X), "Battery history \u2014 BatterySight", font=UI(12), fill=fg)
    for i, glyph in enumerate(("\uE8BB", "\uE922", "\uE921")):  # close, maximize, minimize
        gx = W - 46 * (i + 1) + 18
        d.text((gx * X, 11 * X), glyph, font=ICONS(10), fill=fg)
    # Device list.
    lx0, ly0, lx1, ly1 = 16, 48, 266, 548
    d.rectangle((lx0 * X, ly0 * X, lx1 * X, ly1 * X), outline=(130, 135, 144), width=X)
    devices = ["PRO X Wireless Gaming Headset", "PRO X Superlight", "Xbox Wireless Controller"]
    for i, name in enumerate(devices):
        top = ly0 + 2 + i * 20
        selected = i == 1
        if selected:
            d.rectangle(((lx0 + 1) * X, top * X, (lx1 - 1) * X, (top + 20) * X), fill=(0, 120, 215))
        d.text(((lx0 + 4) * X, (top + 2) * X), name, font=UI(12), fill=(255, 255, 255) if selected else fg)
    # Range buttons, 24 h pressed.
    for i, label in enumerate(("24 h", "7 d", "30 d")):
        bx = 282 + i * 87
        pressed = i == 0
        d.rounded_rectangle(
            (bx * X, 50 * X, (bx + 78) * X, 82 * X),
            4 * X,
            fill=(204, 228, 247) if pressed else (253, 253, 253),
            outline=(0, 84, 153) if pressed else (208, 208, 208),
            width=X,
        )
        f = UI(12)
        d.text(((bx + 39) * X - d.textlength(label, font=f) / 2, 58 * X), label, font=f, fill=fg)
    d.text((282 * X, 98 * X), "Time left: about 46 h", font=UI(12), fill=fg)
    img.paste(chart, (282 * X, (548 - 413) * X))
    d.rounded_rectangle((0, 0, img.width - 1, img.height - 1), 8 * X, outline=(200, 200, 200), width=X)
    return floating(img, 8, blur=20, offset=10, opacity=60)


def social() -> Image.Image:
    """GitHub's social preview (1280x640, in pixels rather than units): the logo, the
    name and the tagline over the dark taskbar screenshot."""
    W, H = 1280, 640
    img = wallpaper(W, H, dark=True)
    d = ImageDraw.Draw(img)
    logo = Image.open(LOGO).convert("RGBA").resize((120, 120), Image.LANCZOS)
    title, tagline = ImageFont.truetype(str(FONTS / "seguisb.ttf"), 76), font("segoeui.ttf", 15)
    name, line = "BatterySight", "Wireless battery levels on the Windows taskbar"
    tw = 120 + 28 + d.textlength(name, font=title)
    x = (W - tw) // 2
    img.alpha_composite(logo, (round(x), 40))
    d.text((x + 148, 100), name, font=title, fill=(255, 255, 255, 255), anchor="lm")
    d.text((W // 2, 196), line, font=tagline, fill=(200, 215, 235, 255), anchor="mm")
    shot = floating(hero(True), 12, blur=24, offset=10, opacity=140)  # wider than the card
    pad = (shot.height - 380) // 2
    img.paste(shot, ((W - shot.width) // 2, H - 30 - 380 - pad), shot)
    return img.convert("RGB")


def main() -> None:
    for dark in (False, True):
        theme = "dark" if dark else "light"
        hero(dark).save(OUT / f"hero-{theme}.png", optimize=True)
        menu(dark).save(OUT / f"menu-{theme}.png", optimize=True)
    history().save(OUT / "history.png", optimize=True)
    social().save(OUT / "social.png", optimize=True)


if __name__ == "__main__":
    main()
