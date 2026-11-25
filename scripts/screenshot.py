#!/usr/bin/env python3
"""Capture assets/screenshot.png deterministically using PTLabel's built-in framebuffer screenshot.

Works identically on local macOS and headless/CI macOS runners (no Screen Recording
permission or AppleScript required). Wraps the rendered UI with a crisp macOS window
titlebar, rounded corners, and soft drop shadow.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parent.parent
OUT_PATH = ROOT / "assets" / "screenshot.png"
TITLE = "PTLabel — parts-labels.ptlabel"
TARGET_WIDTH = 1800


def find_binary() -> Path:
    if env_bin := os.environ.get("PTLABEL_BIN"):
        return Path(env_bin)
    for rel in ("target/release/ptlabel", "target/debug/ptlabel"):
        p = ROOT / rel
        if p.exists():
            return p
    raise SystemExit("ptlabel binary not found; build with `cargo build` or set PTLABEL_BIN")


def load_title_font(size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    candidates = [
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/HelveticaNeue.ttc",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    ]
    for path in candidates:
        if os.path.exists(path):
            try:
                # HelveticaNeue.ttc index 1 is Bold; SFNS.ttf index 0
                idx = 1 if path.endswith("HelveticaNeue.ttc") else 0
                return ImageFont.truetype(path, size, index=idx)
            except Exception:
                try:
                    return ImageFont.truetype(path, size)
                except Exception:
                    pass
    return ImageFont.load_default()


def add_macos_frame(inner: Image.Image, title: str) -> Image.Image:
    inner = inner.convert("RGBA")
    iw, ih = inner.size
    # Infer HiDPI scale (2x on Retina, 1x on standard VM) from captured width (~1280 logical px).
    s = 2 if iw >= 1800 else 1

    title_h = 28 * s
    radius = 10 * s
    win_w = iw
    win_h = ih + title_h

    # Assemble window surface (titlebar + inner UI).
    win = Image.new("RGBA", (win_w, win_h), (242, 242, 242, 255))
    draw = ImageDraw.Draw(win)
    # Subtle macOS light titlebar fill + bottom divider.
    draw.rectangle([0, 0, win_w, title_h], fill=(242, 242, 242, 255))
    draw.line([(0, title_h - 1), (win_w, title_h - 1)], fill=(214, 214, 214, 255), width=max(1, s))

    # macOS traffic lights (red, yellow, green).
    cy = title_h // 2
    r = 6 * s
    lights = [
        (20 * s, (255, 95, 86, 255), (224, 68, 62, 255)),
        (40 * s, (255, 189, 46, 255), (222, 161, 35, 255)),
        (60 * s, (39, 201, 63, 255), (26, 171, 41, 255)),
    ]
    for cx, fill, stroke in lights:
        draw.ellipse([cx - r, cy - r, cx + r, cy + r], fill=fill, outline=stroke, width=max(1, s // 2))

    # Centered title text.
    font = load_title_font(13 * s)
    bbox = draw.textbbox((0, 0), title, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    draw.text(((win_w - tw) // 2, (title_h - th) // 2 - bbox[1]), title, fill=(56, 56, 56, 255), font=font)

    win.paste(inner, (0, title_h))

    # Round all four window corners cleanly.
    mask = Image.new("L", (win_w, win_h), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, win_w - 1, win_h - 1], radius=radius, fill=255)
    rounded = Image.new("RGBA", (win_w, win_h), (0, 0, 0, 0))
    rounded.paste(win, (0, 0), mask)

    # Crisp 1px window outline.
    outline = Image.new("RGBA", (win_w, win_h), (0, 0, 0, 0))
    ImageDraw.Draw(outline).rounded_rectangle(
        [0, 0, win_w - 1, win_h - 1],
        radius=radius,
        outline=(0, 0, 0, 38),
        width=max(1, s),
    )
    rounded = Image.alpha_composite(rounded, outline)

    # Soft macOS drop shadow canvas.
    pad_x = 56 * s
    pad_top = 36 * s
    pad_bot = 72 * s
    canvas = Image.new("RGBA", (win_w + 2 * pad_x, win_h + pad_top + pad_bot), (0, 0, 0, 0))

    shadow = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    sdraw = ImageDraw.Draw(shadow)
    sdraw.rounded_rectangle(
        [pad_x, pad_top + 16 * s, pad_x + win_w - 1, pad_top + 16 * s + win_h - 1],
        radius=radius,
        fill=(0, 0, 0, 82),
    )
    shadow = shadow.filter(ImageFilter.GaussianBlur(radius=22 * s))
    canvas.alpha_composite(shadow)
    canvas.alpha_composite(rounded, (pad_x, pad_top))
    return canvas


def main() -> None:
    binary = find_binary()
    xlsx = str((ROOT / "examples" / "parts.xlsx").resolve())
    profile_name = "readme-shot"

    if sys.platform == "darwin":
        app_support = Path.home() / "Library" / "Application Support" / f"PTLabel-{profile_name}"
    else:
        xdg = os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local" / "share"))
        app_support = Path(xdg) / f"PTLabel-{profile_name}"

    with tempfile.TemporaryDirectory(prefix="ptlabel_shot_") as tmpdir:
        tmp = Path(tmpdir)
        doc_path = tmp / "parts-labels.ptlabel"
        raw_shot = tmp / "inner.png"

        # Save matching .ptlabel JSON so window title has no "— Edited".
        label_json = {
            "blocks": [
                {"Qr": {"data": "{Part} · {Value} · {Location}", "ec": "M"}},
                {
                    "Text": {
                        "text": "{Part} · {Value}\n{Package} · Bin {Location}",
                        "font": "",
                        "size_pt": None,
                        "bold": True,
                        "italic": False,
                        "underline": False,
                        "align": "Center",
                        "line_spacing": 1.0,
                        "rotation": "R0",
                    }
                },
            ],
            "gap_mm": 3.0,
            "frame": True,
            "invert": False,
            "fixed_length": False,
            "length_mm": 50.0,
            "align": "Center",
            "data": {
                "path": xlsx,
                "sheet": "Parts",
                "header": True,
                "header_row": None,
                "fill_merged": True,
                "by_columns": False,
            },
        }
        doc_path.write_text(json.dumps(label_json, ensure_ascii=False, indent=2), encoding="utf-8")

        def ron_str(s: str) -> str:
            return s.replace("\\", "\\\\").replace('"', '\\"')

        ron_inner = (
            "(label:(blocks:["
            'Qr((data:"{Part} · {Value} · {Location}",ec:M)),'
            'Text((text:"{Part} · {Value}\\n{Package} · Bin {Location}",font:"",size_pt:None,bold:true,italic:false,underline:false,align:Center,line_spacing:1.0,rotation:R0))'
            "],gap_mm:3.0,frame:true,invert:false,fixed_length:false,length_mm:50.0,align:Center,"
            f'data:Some((path:"{ron_str(xlsx)}",sheet:"Parts",header:true,header_row:None,fill_merged:true,by_columns:false))),'
            "settings:(copies:1,infinite:false,opts:(auto_cut:true,half_cut:false,chain:false,cut_every:1,mirror:false,high_res:false,special_tape:false,margin_mm:2.0),num_start:1,num_step:1),"
            f'printer:None,auto_connect:false,hosts:[],offline_tape:18,doc_path:Some("{ron_str(str(doc_path))}"),recent:[],sel_rows:[0,1,2,3],preview_height:205.0)'
        )
        ron_outer = f'{{\n    "ptlabel": "{ron_str(ron_inner)}",\n}}\n'

        if app_support.exists():
            shutil.rmtree(app_support)
        app_support.mkdir(parents=True, exist_ok=True)
        (app_support / "app.ron").write_text(ron_outer, encoding="utf-8")

        env = os.environ.copy()
        env["PTLABEL_PROFILE"] = profile_name
        env["PTLABEL_SCREENSHOT"] = str(raw_shot)
        env["PTLABEL_WINDOW_SIZE"] = "1240x820"
        env["PTLABEL_ZOOM"] = "0.94"

        try:
            subprocess.run([str(binary)], env=env, check=True, timeout=45)
        finally:
            shutil.rmtree(app_support, ignore_errors=True)

        if not raw_shot.exists():
            raise SystemExit(f"Screenshot was not written to {raw_shot}")

        framed = add_macos_frame(Image.open(raw_shot), TITLE)
        if framed.width > TARGET_WIDTH:
            new_h = round(framed.height * TARGET_WIDTH / framed.width)
            framed = framed.resize((TARGET_WIDTH, new_h), Image.Resampling.LANCZOS)

        OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
        framed.save(OUT_PATH, format="PNG", optimize=True)

        if shutil.which("pngquant"):
            subprocess.run(
                ["pngquant", "--force", "--quality=80-95", "--output", str(OUT_PATH), str(OUT_PATH)],
                check=False,
            )

        print(f"Saved {OUT_PATH} ({framed.width}x{framed.height}, {OUT_PATH.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
