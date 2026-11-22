"""Spike 1 scoring: CIEDE2000 between the direct Rust render and each preview.

    python3 -I spike/compare.py <dir> [mode ...]

Compares, at the proxy's size:
- the Rust trilinear model of the LUT (LUT error alone),
- what WebGL2 drew (LUT error, GPU half-float filtering and 8-bit output),
against `direct.f32` quantized to 8 bits, which is what a display shows.
"""

import json
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from de2000 import compare  # noqa: E402

d = sys.argv[1]
modes = sys.argv[2:] or ["swiftshader"]
meta = json.load(open(os.path.join(d, "spike.json")))
w, h = meta["width"], meta["height"]
direct = np.fromfile(os.path.join(d, "direct.f32"), dtype="<f4").reshape(h, w, 3)
truth = np.round(np.clip(direct, 0, 1) * 255) / 255
print(f"{w}x{h}, proxy {meta['proxy_bytes']} bytes")
for size in (17, 33):
    model = np.fromfile(os.path.join(d, f"model-{size}.f32"), dtype="<f4").reshape(h, w, 3)
    m = np.round(np.clip(model, 0, 1) * 255) / 255
    print(f"  rust model {size}^3: mean {compare(m, truth)[0]:.3f} p99 {compare(m, truth)[1]:.3f} max {compare(m, truth)[2]:.2f}")
    for mode in modes:
        f = os.path.join(d, f"webgl-{size}-{mode}.rgba")
        if not os.path.exists(f):
            continue
        g = np.fromfile(f, dtype=np.uint8).reshape(h, w, 4)[..., :3] / 255
        mean, p99, mx = compare(g, truth)
        codes = np.abs(g * 255 - truth * 255)
        print(f"  webgl {mode} {size}^3: mean {mean:.3f} p99 {p99:.3f} max {mx:.2f}; code diff max {codes.max():.0f}, >2: {(codes > 2).mean() * 100:.3f}%")
