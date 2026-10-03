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
