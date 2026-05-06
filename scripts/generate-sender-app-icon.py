#!/usr/bin/env python3

from pathlib import Path

from PIL import Image, ImageChops, ImageDraw, ImageFilter


SIZE = 1024
OUTPUT_PATH = Path(__file__).resolve().parent.parent / 'sender' / 'assets' / 'app-icon-1024.png'


def draw_gradient_background() -> Image.Image:
    image = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    gradient = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(gradient)
    for y in range(SIZE):
        t = y / (SIZE - 1)
        r = int(18 + (20 * (1 - t)))
        g = int(24 + (30 * (1 - t)))
        b = int(36 + (48 * t))
        draw.line((0, y, SIZE, y), fill=(r, g, b, 255))

    mask = Image.new('L', (SIZE, SIZE), 0)
    ImageDraw.Draw(mask).rounded_rectangle((72, 72, SIZE - 72, SIZE - 72), radius=230, fill=255)
    image = Image.composite(gradient, image, mask)

    glow = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    ImageDraw.Draw(glow).rounded_rectangle(
        (96, 96, SIZE - 96, SIZE - 96),
        radius=210,
        outline=(255, 255, 255, 34),
        width=8,
    )
    return Image.alpha_composite(image, glow)


def draw_ring() -> Image.Image:
    ring = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(ring)
    center = SIZE / 2
    outer = 280
    inner = 210
    draw.ellipse(
        (center - outer, center - outer, center + outer, center + outer),
        fill=(210, 222, 238, 255),
    )
    draw.ellipse(
        (center - inner, center - inner, center + inner, center + inner),
        fill=(22, 28, 36, 255),
    )
    return ring


def draw_arrows() -> Image.Image:
    arrows = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(arrows)
    color = (243, 247, 252, 255)
    center = SIZE / 2
    shaft_h = 40

    cy = center - 105
    shaft_left = center - 185
    shaft_right = shaft_left + 290
    shaft_top = cy - shaft_h / 2
    draw.rounded_rectangle((shaft_left, shaft_top, shaft_right, shaft_top + shaft_h), radius=20, fill=color)
    draw.polygon([(center + 180, cy), (center + 78, cy - 78), (center + 78, cy + 78)], fill=color)

    cy = center + 105
    shaft_left = center - 105
    shaft_right = center + 185
    shaft_top = cy - shaft_h / 2
    draw.rounded_rectangle((shaft_left, shaft_top, shaft_right, shaft_top + shaft_h), radius=20, fill=color)
    draw.polygon([(center - 180, cy), (center - 78, cy - 78), (center - 78, cy + 78)], fill=color)
    return arrows


def draw_highlights() -> Image.Image:
    sheen = Image.new('RGBA', (SIZE, SIZE), (0, 0, 0, 0))
    ImageDraw.Draw(sheen).ellipse((180, 140, 520, 440), fill=(255, 255, 255, 20))
    return sheen.filter(ImageFilter.GaussianBlur(24))


def main() -> None:
    background = draw_gradient_background()
    ring = draw_ring()
    arrows = draw_arrows()

    shadow = Image.eval(arrows.filter(ImageFilter.GaussianBlur(18)), lambda px: int(px * 0.35))

    image = Image.alpha_composite(background, ring)
    image = Image.alpha_composite(image, shadow)
    image = Image.alpha_composite(image, arrows)
    image = Image.alpha_composite(image, draw_highlights())

    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    image.save(OUTPUT_PATH)
    print(OUTPUT_PATH)


if __name__ == '__main__':
    main()
