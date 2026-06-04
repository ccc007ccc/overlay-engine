#!/usr/bin/env python3
"""生成 overlay-engine 图标资产。"""

from __future__ import annotations

import math
from pathlib import Path
from typing import Iterable

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[1]
ASSET_DIR = ROOT / "monitors" / "game-bar-widget" / "Assets"
CORE_ICO = ROOT / "core-server" / "resources" / "overlay-core.ico"
DESKTOP_ICO = ROOT / "monitors" / "desktop-window" / "resources" / "overlay-desktop-monitor.ico"

SCALES = {100: 1.0, 125: 1.25, 150: 1.5, 200: 2.0, 400: 4.0}
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

THEMES = {
    "core": {
        "page": "#f8fafc",
        "window": "#ffffff",
        "stroke": "#111827",
        "dot2": "#4b5563",
        "dot3": "#9ca3af",
    },
    "desktop": {
        "page": "#dbeafe",
        "window": "#eff6ff",
        "stroke": "#1d4ed8",
        "dot2": "#60a5fa",
        "dot3": "#93c5fd",
        "symbol": "#1e40af",
        "symbol_fill": "#bfdbfe",
    },
    "gamebar": {
        "page": "#dcfce7",
        "window": "#ecfdf5",
        "stroke": "#16a34a",
        "dot2": "#22c55e",
        "dot3": "#86efac",
        "symbol": "#f0fdf4",
        "symbol_fill": "#16a34a",
    },
}


def s(value: float, size: int) -> int:
    return round(value * size / 1024)


def scaled_box(box: tuple[float, float, float, float], size: int) -> tuple[int, int, int, int]:
    return tuple(s(v, size) for v in box)  # type: ignore[return-value]


def line_width(value: float, size: int) -> int:
    return max(1, s(value, size))


def rounded_rect(draw: ImageDraw.ImageDraw, box: tuple[float, float, float, float], radius: float, size: int, **kwargs) -> None:
    draw.rounded_rectangle(scaled_box(box, size), radius=s(radius, size), **kwargs)


def draw_base(size: int, theme: dict[str, str]) -> tuple[Image.Image, ImageDraw.ImageDraw]:
    image = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    draw = ImageDraw.Draw(image)
    rounded_rect(draw, (0, 0, 1024, 1024), 224, size, fill=theme["page"])
    rounded_rect(
        draw,
        (144, 220, 880, 804),
        72,
        size,
        fill=theme["window"],
        outline=theme["stroke"],
        width=line_width(36, size),
    )
    draw.line(scaled_box((144, 344, 880, 344), size), fill=theme["stroke"], width=line_width(36, size))
    for x, color in ((228, theme["stroke"]), (298, theme["dot2"]), (368, theme["dot3"])):
        r = s(22, size)
        cx = s(x, size)
        cy = s(282, size)
        draw.ellipse((cx - r, cy - r, cx + r, cy + r), fill=color)
    return image, draw


def gear_points(cx: float, cy: float, inner: float, outer: float, teeth: int = 10) -> list[tuple[float, float]]:
    points: list[tuple[float, float]] = []
    for i in range(teeth * 2):
        radius = outer if i % 2 == 0 else inner
        angle = -math.pi / 2 + i * math.pi / teeth
        points.append((cx + math.cos(angle) * radius, cy + math.sin(angle) * radius))
    return points


def draw_core(size: int) -> Image.Image:
    image, draw = draw_base(size, THEMES["core"])
    fill = THEMES["core"]["stroke"]
    points = [(s(x, size), s(y, size)) for x, y in gear_points(512, 608, 104, 164, 12)]
    draw.polygon(points, fill=fill)
    r_outer = s(76, size)
    r_inner = s(36, size)
    c = s(512, size), s(608, size)
    draw.ellipse((c[0] - r_outer, c[1] - r_outer, c[0] + r_outer, c[1] + r_outer), fill=THEMES["core"]["window"])
    draw.ellipse((c[0] - r_inner, c[1] - r_inner, c[0] + r_inner, c[1] + r_inner), fill=fill)
    return image


def draw_desktop(size: int) -> Image.Image:
    image, draw = draw_base(size, THEMES["desktop"])
    theme = THEMES["desktop"]
    rounded_rect(
        draw,
        (318, 452, 706, 684),
        36,
        size,
        fill=theme["symbol_fill"],
        outline=theme["symbol"],
        width=line_width(44, size),
    )
    draw.line(scaled_box((448, 748, 576, 748), size), fill=theme["symbol"], width=line_width(44, size))
    draw.line(scaled_box((512, 684, 512, 748), size), fill=theme["symbol"], width=line_width(44, size))
    draw.line(scaled_box((378, 526, 646, 526), size), fill="#60a5fa", width=line_width(28, size))
    draw.line(scaled_box((378, 590, 562, 590), size), fill="#60a5fa", width=line_width(28, size))
    return image


def draw_gamebar(size: int) -> Image.Image:
    image, draw = draw_base(size, THEMES["gamebar"])
    theme = THEMES["gamebar"]
    c = s(512, size), s(606, size)
    r = s(150, size)
    draw.ellipse((c[0] - r, c[1] - r, c[0] + r, c[1] + r), fill=theme["symbol_fill"])
    width = line_width(38, size)
    draw.arc(scaled_box((382, 440, 642, 654), size), 200, 340, fill=theme["symbol"], width=width)
    draw.arc(scaled_box((374, 558, 650, 816), size), 205, 335, fill=theme["symbol"], width=width)
    draw.line(scaled_box((392, 542, 512, 670), size), fill=theme["symbol"], width=line_width(34, size))
    draw.line(scaled_box((632, 542, 512, 670), size), fill=theme["symbol"], width=line_width(34, size))
    return image


def save_png(path: Path, image: Image.Image) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, "PNG")


def save_ico(path: Path, factory) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    base = factory(256)
    sizes = [(n, n) for n in ICO_SIZES]
    base.save(path, format="ICO", sizes=sizes)


def scaled_asset_size(base: int, multiplier: float) -> int:
    return math.ceil(base * multiplier)


def generate_scaled_assets(prefix: str, base_w: int, base_h: int, factory) -> None:
    for scale, multiplier in SCALES.items():
        size = (scaled_asset_size(base_w, multiplier), scaled_asset_size(base_h, multiplier))
        image = factory(max(size)).resize(size, Image.Resampling.LANCZOS)
        save_png(ASSET_DIR / f"{prefix}.scale-{scale}.png", image)


def generate_square_target_assets(prefixes: Iterable[str], sizes: Iterable[int], factory) -> None:
    for prefix in prefixes:
        for size in sizes:
            save_png(ASSET_DIR / f"{prefix}{size}.png", factory(size))


def main() -> None:
    save_ico(CORE_ICO, draw_core)
    save_ico(DESKTOP_ICO, draw_desktop)

    generate_scaled_assets("StoreLogo", 50, 50, draw_gamebar)
    generate_scaled_assets("Square44x44Logo", 44, 44, draw_gamebar)
    generate_scaled_assets("Square150x150Logo", 150, 150, draw_gamebar)
    generate_scaled_assets("SmallTile", 71, 71, draw_gamebar)
    generate_scaled_assets("LargeTile", 310, 310, draw_gamebar)
    generate_scaled_assets("Wide310x150Logo", 310, 150, draw_gamebar)
    generate_scaled_assets("SplashScreen", 620, 300, draw_gamebar)
    save_png(ASSET_DIR / "LockScreenLogo.scale-200.png", draw_gamebar(48))

    generate_square_target_assets(("Square44x44Logo.targetsize-",), (16, 24, 32, 48, 256), draw_gamebar)
    generate_square_target_assets(("Square44x44Logo.altform-unplated_targetsize-",), (16, 32, 48, 256), draw_gamebar)
    save_png(ASSET_DIR / "Square44x44Logo.targetsize-24_altform-unplated.png", draw_gamebar(24))

    print(f"generated {CORE_ICO}")
    print(f"generated {DESKTOP_ICO}")
    print(f"generated Game Bar assets in {ASSET_DIR}")


if __name__ == "__main__":
    main()
