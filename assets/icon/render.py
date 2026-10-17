"""Builds the app icons from assets/icon/birdman64-source.png (pip: pillow).
Run from the repo root. Sizes >= 32 px use the full artwork; 16/24 px use a
crop of the head, because the full scene turns to mud that small."""
from PIL import Image

src = Image.open("assets/icon/birdman64-source.png").convert("RGBA")
head = src.crop((250, 310, 820, 880))


def icon(n):
    return (head if n < 32 else src).resize((n, n), Image.LANCZOS)


icon(512).save("assets/icon/birdman64.png")
icon(256).save("packaging/linux/birdman64.png")
# Title-bar/taskbar icon: decoded by crates/birdman64/src/window.rs (winit Icon).
icon(64).save("assets/icon/birdman64-64.png")
sizes = [16, 24, 32, 48, 64, 128, 256]
icon(256).save("assets/icon/birdman64.ico", sizes=[(s, s) for s in sizes],
               append_images=[icon(s) for s in sizes[:-1]])

# Android launcher icon (packaging/android/res, compiled by build-apk.sh).
# Adaptive layers are 108 dp; launchers show about the middle 72 dp through
# a circle or squircle mask. The artwork is 64 dp on the foreground so the
# "B64" corner stays inside a circular mask; the background is the same art
# with its edge pixels repeated out to 108 dp and blurred (the thin ring
# around the art, and what shows when a launcher moves the layers).
# Legacy mipmaps (48 dp) are the plain artwork.
import numpy as np
from PIL import ImageFilter

res = "packaging/android/res"
for name, scale in [("mdpi", 1), ("hdpi", 1.5), ("xhdpi", 2), ("xxhdpi", 3), ("xxxhdpi", 4)]:
    d = f"{res}/mipmap-{name}"
    import os
    os.makedirs(d, exist_ok=True)
    icon(round(48 * scale)).save(f"{d}/ic_launcher.png")
    layer, art = round(108 * scale), round(64 * scale)
    pad = (layer - art) // 2
    fg = Image.new("RGBA", (layer, layer), (0, 0, 0, 0))
    fg.paste(icon(art), (pad, pad))
    fg.save(f"{d}/ic_launcher_foreground.png")
    rgb = np.asarray(icon(art).convert("RGB"))
    edge = np.pad(rgb, ((pad, layer - art - pad), (pad, layer - art - pad), (0, 0)), mode="edge")
    bg = Image.fromarray(edge).filter(ImageFilter.GaussianBlur(scale * 3))
    bg.save(f"{d}/ic_launcher_background.png")
