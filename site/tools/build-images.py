"""Regenerate the website's optimized images from the marketing sources.

Run from the repository root:  python site/tools/build-images.py
Needs Pillow >= 11 (WebP + AVIF support). Output: site/assets/img/ and site/ (favicons).
"""
import os, sys
from PIL import Image

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
RAW = os.path.join(ROOT, "marketing", "src", "raw")
OUT = os.path.join(ROOT, "site", "assets", "img")
SITE = os.path.join(ROOT, "site")
os.makedirs(OUT, exist_ok=True)

# name -> (source file stem, crop box in source px or None, output widths)
SHOTS = {
    "main":      ("main",      None,                   [2400, 1600, 960, 640]),
    "details":   ("details",   None,                   [1140]),
    "video":     ("video",     None,                   [1020]),
    "capture":   ("capture",   None,                   [900]),
    "complete":  ("complete",  None,                   [750]),
    "mini":      ("mini",      None,                   [720]),
    "scheduler": ("scheduler", (561, 204, 2319, 1596), [1100]),
    "grabber":   ("grabber",   (540, 201, 2340, 1599), [1100]),
    "settings":  ("settings",  (402, 180, 2481, 1620), [1100]),
    # Hugging Face model downloader (1.1.7): the index feature and site/hugging-face/.
    "hf-model":  ("hf-model",  None,                   [1000]),
    "hf-search": ("hf-search", None,                   [1000]),
    "hf-queue":  ("hf-queue",  None,                   [1600, 960, 640]),
}

def save(im, stem, avif=True):
    im.save(os.path.join(OUT, stem + ".webp"), "WEBP", quality=82, method=6)
    if avif:
        im.save(os.path.join(OUT, stem + ".avif"), "AVIF", quality=60, speed=4)

def fit(im, w):
    if im.width <= w:
        return im
    return im.resize((w, round(im.height * w / im.width)), Image.LANCZOS)

for name, (src, box, widths) in SHOTS.items():
    for theme in ("dark", "light"):
        im = Image.open(os.path.join(RAW, f"{src}-{theme}.png")).convert("RGB")
        if box:
            im = im.crop(box)
        for i, w in enumerate(widths):
            out = fit(im, w)
            stem = f"{name}-{theme}" + ("" if i == 0 else f"-{w}")
            save(out, stem)
            print(stem, out.size)

# Film poster: only rebuilt from the real poster frame (the committed fallback poster was
# rendered from the hero scene; make-hero-video.ps1 also refreshes it).
poster_src = os.path.join(ROOT, "marketing", "video", "prism-promo-poster-frame.png")
if os.path.exists(poster_src):
    im = Image.open(poster_src).convert("RGB")
    tw = im.width; th = round(tw * 9 / 16)
    if th > im.height:
        th = im.height; tw = round(th * 16 / 9)
    left = (im.width - tw) // 2; top = (im.height - th) // 2
    im = im.crop((left, top, left + tw, top + th)).resize((1280, 720), Image.LANCZOS)
    save(im, "film-poster")
    print("film-poster", im.size)

# Hero key visual (dark theme only; see index.html data-hero-art). 3D render from Blender.
KV = os.path.join(ROOT, "marketing", "keyvisual", "prism-kv-04-burst-3840x2160.png")
if os.path.exists(KV):
    kv = Image.open(KV).convert("RGB")
    for w, suffix in ((2400, ""), (1280, "-1280")):
        r = kv.resize((w, round(kv.height * w / kv.width)), Image.LANCZOS)
        r.save(os.path.join(OUT, f"keyvisual{suffix}.webp"), "WEBP", quality=82, method=6)
        r.save(os.path.join(OUT, f"keyvisual{suffix}.avif"), "AVIF", quality=66, speed=4)
    print("keyvisual", KV)

# Open Graph image (must stay PNG/JPEG for social crawlers)
og = Image.open(os.path.join(ROOT, "marketing", "og.png")).convert("RGB")

og.save(os.path.join(SITE, "og.png"), "PNG", optimize=True)

# Favicons
icon = Image.open(os.path.join(ROOT, "assets", "brand", "png", "app-icon-512.png")).convert("RGBA")
icon.resize((180, 180), Image.LANCZOS).save(os.path.join(SITE, "apple-touch-icon.png"), optimize=True)
icon.resize((192, 192), Image.LANCZOS).save(os.path.join(SITE, "assets", "icons", "icon-192.png"), optimize=True)
icon.save(os.path.join(SITE, "assets", "icons", "icon-512.png"), optimize=True)
small = Image.open(os.path.join(ROOT, "assets", "brand", "png", "app-icon-32.png")).convert("RGBA")
small.save(os.path.join(SITE, "favicon-32.png"), optimize=True)
big48 = Image.open(os.path.join(ROOT, "assets", "brand", "png", "app-icon-48.png")).convert("RGBA")
big48.save(os.path.join(SITE, "favicon.ico"), sizes=[(16, 16), (32, 32), (48, 48)])
app256 = Image.open(os.path.join(ROOT, "assets", "brand", "png", "app-icon-256.png")).convert("RGBA")
app256.resize((160, 160), Image.LANCZOS).save(os.path.join(OUT, "app-icon.webp"), "WEBP", quality=90, method=6)
print("done")
