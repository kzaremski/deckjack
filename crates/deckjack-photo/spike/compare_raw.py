"""Spike 2 on a real RAW: CIEDE2000 of each 8-bit log proxy's developed result
against the f32 reference (output 8-bit codes, as written by
`examples/banding.rs <raw> <dir>`)."""

import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from de2000 import compare  # noqa: E402

d = sys.argv[1]
w, h = map(int, open(os.path.join(d, "size")).read().split())


def load(name):
    return np.fromfile(os.path.join(d, f"{name}.f32"), dtype="<f4").reshape(h, w, 3)


ref = load("reference")
for name in ("log8", "log8-dither"):
    mean, p99, mx = compare(load(name), ref)
    print(f"{name}: dE2000 mean {mean:.3f} p99 {p99:.3f} max {mx:.2f}")
