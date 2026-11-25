#!/usr/bin/env python3
"""Generate the PTLabel app icon: assets/icon.png (512, window icon) + assets/PTLabel.icns.

Same geometry as icons::paint_logo (unit square of the tile):
tile radius .225, tape strip edge to edge at y .35-.65, text bar .34-.66 x .455-.545,
cut notches + hairline at x=.22 and .78. Needs Pillow; pngquant (brew) shrinks the icns ~3x.
"""
import os
import shutil
import struct
import subprocess
import tempfile

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
SS = 4  # supersampling
N = 1024 * SS
BODY0, BODY = 100 * SS, 824 * SS  # Apple icon grid: 824 px tile, 100 px margin
TOP, BOTTOM = (76, 144, 248), (40, 102, 220)
SHADOW_COL = (10, 30, 80)
DEV_TOP, DEV_BOTTOM = (250, 206, 52), (226, 154, 14)
DEV_SHADOW_COL = (95, 58, 8)
INK = (28, 37, 51)


def u(v):
    return BODY0 + v * BODY


def box(x0, y0, x1, y1):
    return [u(x0), u(y0), u(x1), u(y1)]


def make(top=TOP, bottom=BOTTOM, shadow_col=SHADOW_COL):
    img = Image.new("RGBA", (N, N), (0, 0, 0, 0))

    # soft drop shadow
    sh = Image.new("L", (N, N), 0)
    ImageDraw.Draw(sh).rounded_rectangle(box(0, 0.02, 1, 1.02), radius=0.225 * BODY, fill=110)
    sh = sh.filter(ImageFilter.GaussianBlur(14 * SS))
    img.paste((0, 0, 0, 255), (0, 0), sh)

    # tile with vertical gradient
    grad = Image.new("RGBA", (1, N))
    for y in range(N):
        t = min(max((y - BODY0) / BODY, 0), 1)
        grad.putpixel((0, y), tuple(round(a + (b - a) * t) for a, b in zip(top, bottom)) + (255,))
    grad = grad.resize((N, N))
    mask = Image.new("L", (N, N), 0)
    ImageDraw.Draw(mask).rounded_rectangle(box(0, 0, 1, 1), radius=0.225 * BODY, fill=255)
    img.paste(grad, (0, 0), mask)

    # tape strip running edge to edge (clipped to the tile), with a cut on each side
    layer = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    s2 = Image.new("L", (N, N), 0)
    ImageDraw.Draw(s2).rectangle(box(-0.1, 0.37, 1.1, 0.67), fill=80)
    s2 = s2.filter(ImageFilter.GaussianBlur(8 * SS))
    layer.paste(shadow_col + (255,), (0, 0), s2)
    d = ImageDraw.Draw(layer)
    d.rectangle(box(-0.1, 0.35, 1.1, 0.65), fill=(255, 255, 255, 255))
    d.rounded_rectangle(box(0.34, 0.455, 0.66, 0.545), radius=0.045 * BODY, fill=INK + (255,))
    for x in (0.22, 0.78):
        col = tuple(round(a + (b - a) * 0.5) for a, b in zip(top, bottom)) + (255,)
        d.polygon([(u(x - 0.045), u(0.35)), (u(x + 0.045), u(0.35)), (u(x), u(0.405))], fill=col)
        d.polygon([(u(x - 0.045), u(0.65)), (u(x + 0.045), u(0.65)), (u(x), u(0.595))], fill=col)
        d.rectangle(box(x - 0.006, 0.40, x + 0.006, 0.60), fill=col)
    clip = Image.new("L", (N, N), 0)
    clip.paste(layer.getchannel("A"), (0, 0), mask)
    layer.putalpha(clip)
    img.alpha_composite(layer)

    return img.resize((1024, 1024), Image.Resampling.LANCZOS)


def write_set(icon, png_name, icns_name, ico_name=None):
    os.makedirs(os.path.join(ROOT, "assets"), exist_ok=True)
    png_path = os.path.join(ROOT, "assets", png_name)
    icon.resize((512, 512), Image.Resampling.LANCZOS).save(png_path, optimize=True)
    if shutil.which("pngquant"):
        subprocess.run(["pngquant", "--quality=80-100", "--speed=1", "--strip", "--force", "--ext", ".png", png_path], check=True)
    # .icns = "icns" + length, then (type, length, PNG) entries. Written directly so the
    # PNGs stay palette-compressed (iconutil re-encodes them as 32-bit).
    types = [("icp4", 16), ("icp5", 32), ("ic07", 128), ("ic08", 256), ("ic09", 512),
             ("ic11", 32), ("ic12", 64), ("ic13", 256), ("ic14", 512), ("ic10", 1024)]
    entries = []
    with tempfile.TemporaryDirectory() as tmp:
        for t, px in types:
            f = os.path.join(tmp, f"{t}.png")
            icon.resize((px, px), Image.Resampling.LANCZOS).save(f)
            if shutil.which("pngquant"):  # flat artwork: 256 colours look identical, ~3x smaller
                subprocess.run(["pngquant", "--quality=80-100", "--speed=1", "--strip", "--force", "--ext", ".png", f], check=True)
            data = open(f, "rb").read()
            entries.append(t.encode() + struct.pack(">I", 8 + len(data)) + data)
    body = b"".join(entries)
    with open(os.path.join(ROOT, "assets", icns_name), "wb") as out:
        out.write(b"icns" + struct.pack(">I", 8 + len(body)) + body)
    if ico_name:
        icon.save(
            os.path.join(ROOT, "assets", ico_name),
            format="ICO",
            sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
        )


def main():
    write_set(make(TOP, BOTTOM, SHADOW_COL), "icon.png", "PTLabel.icns", "PTLabel.ico")
    write_set(make(DEV_TOP, DEV_BOTTOM, DEV_SHADOW_COL), "icon-dev.png", "PTLabel-Dev.icns")
    print("wrote assets/icon.png, PTLabel.icns, PTLabel.ico, icon-dev.png, PTLabel-Dev.icns")


if __name__ == "__main__":
    main()
