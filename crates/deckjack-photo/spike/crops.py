"""Spike 2: side-by-side crops (reference | log8 | log8 + dither) of a smooth
region, to look at banding rather than only score it.

    python3 -I spike/crops.py <dir> x y w h out.png
"""

import os
import sys

import numpy as np
from PIL import Image

d = sys.argv[1]
x, y, cw, ch = map(int, sys.argv[2:6])
w, h = map(int, open(os.path.join(d, "size")).read().split())
tiles = []
for name in ("reference", "log8", "log8-dither"):
    a = np.fromfile(os.path.join(d, f"{name}.f32"), dtype="<f4").reshape(h, w, 3)
    tiles.append(np.round(np.clip(a[y : y + ch, x : x + cw], 0, 1) * 255).astype(np.uint8))
Image.fromarray(np.concatenate(tiles, axis=1)).save(sys.argv[6])
