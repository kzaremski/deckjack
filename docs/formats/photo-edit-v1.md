# Photo edit sidecar, version 1

`deckjack.photo-edit/1`. Written by the Photos app (`crates/deckjack-photo`).
This page is enough to reimplement the look of an edit without Deckjack: the
file layout, the order of operations, the math of each one and the color
spaces they work in.

## The file

A photo's edit lives beside it as `<photo file name>.deckjack.json`, for
example `IMG_1234.CR2.deckjack.json`. The app never writes `.xmp` names
(Lightroom and darktable own them) and never writes the photo itself.

It is UTF-8 JSON, indented with two spaces, keys in the order below.

```json
{
  "schema": "deckjack.photo-edit/1",
  "original": { "name": "IMG_1234.CR2", "size": 27525027, "sha256": "9f…" },
  "decoder": "rawler 0.8.0",
  "rev": 7,
  "recipe": { … },
  "history": [
    { "label": "Original", "at": 1795300000, "recipe": { … } },
    { "label": "Exposure", "at": 1795300042, "recipe": { … } }
  ],
  "position": 1
}
```

| Field | Meaning |
| --- | --- |
| `schema` | Always `deckjack.photo-edit/1` for this version. A reader refuses any other value. |
| `original` | The photo the edit was made for: file name, size in bytes, SHA-256 of its bytes (hex). A sidecar whose photo no longer matches is reported, not hidden; the recipe is still offered. |
| `decoder` | What decoded the photo: `image 0.25` for JPEG/PNG/WebP, `tiff 0.11` for TIFF, `rawler 0.8.0` for RAW. A RAW developed by another decoder can look different before any edit. |
| `rev` | Revision, +1 on every write. A save names the revision it was based on and is refused when the file has moved on ("edited elsewhere since you opened it"). |
| `recipe` | The current edit. Always equal to `history[position].recipe`. |
| `history` | Every state, oldest first, at most 100. Entry 0 is the photo as it came (the empty recipe) until it ages out. `at` is Unix seconds. |
| `position` | Which entry is current. Below the last after an undo; a new edit drops the entries after it. |

## The recipe

Every field has a default that means "no change", so `{}` is the identity
edit. Unknown fields are refused, not ignored.

```json
{
  "exposure_ev": 0.7,
  "white_balance": { "temperature": 30, "tint": -10 },
  "levels": {
    "master": { "black": 0.02, "white": 0.97, "gamma": 1.1, "out_black": 0, "out_white": 1 },
    "red":    { "black": 0, "white": 1, "gamma": 1, "out_black": 0, "out_white": 1 },
    "green":  { … }, "blue": { … }
  },
  "curves": { "master": [[0, 0], [0.25, 0.18], [0.75, 0.85], [1, 1]], "red": [], "green": [], "blue": [] },
  "saturation": 0.1,
  "geometry": {
    "quarter_turns": 0, "flip_h": false, "flip_v": false,
    "straighten_deg": 3.0,
    "crop": { "x": 0.1, "y": 0.05, "w": 0.6, "h": 0.75 }
  },
  "masks": [
    { "mask": { "kind": "radial", "cx": 0.45, "cy": 0.4, "rx": 0.12, "ry": 0.16, "angle_deg": 0, "feather": 0.6 },
      "invert": false, "exposure_ev": 0.7, "saturation": 0 }
  ]
}
```

| Field | Range | Default |
| --- | --- | --- |
| `exposure_ev` | -5 to 5 stops | 0 |
| `white_balance.temperature`, `.tint` | -100 to 100 | 0 |
| `levels.*.black`, `.white` | 0 to 1, white above black by at least 0.004 | 0, 1 |
| `levels.*.gamma` | 0.1 to 10 | 1 |
| `levels.*.out_black`, `.out_white` | 0 to 1 | 0, 1 |
| `curves.*` | `[]`, or 2 to 16 `[input, output]` points, inputs strictly increasing, all 0 to 1 | `[]` |
| `saturation` | -1 to 1 | 0 |
| `geometry.quarter_turns` | 0 to 3 | 0 |
| `geometry.straighten_deg` | -45 to 45 | 0 |
| `geometry.crop` | each 0 to 1, `w` and `h` at least 0.01, inside the image | full |
| `masks` | at most 4 | `[]` |
| `masks[].mask.cx`, `.cy` | -1 to 2 | — |
| `masks[].mask.rx`, `.ry` | 0.001 to 2 | — |
| `masks[].mask.angle_deg` | -360 to 360 | 0 |
| `masks[].mask.feather` | 0 to 1 | — |
| `masks[].exposure_ev` | -5 to 5 | 0 |
| `masks[].saturation` | -1 to 1 | 0 |

## Color spaces

- **Linear**: linear light with sRGB (Rec. 709) primaries and D65 white.
  Values may exceed 1 before step 3 below.
- **Display**: the sRGB transfer function (IEC 61966-2-1) applied to linear
  values clipped to 0..1:
  `encode(v) = 12.92 v` for `v ≤ 0.0031308`, else `1.055 v^(1/2.4) − 0.055`.
  `decode` is its inverse (`c ≤ 0.04045`: `c / 12.92`, else
  `((c + 0.055) / 1.055)^2.4`).

Decoding a photo:

- JPEG, PNG, WebP: the file's values are taken as display sRGB and decoded
  to linear. An embedded ICC profile is not read in version 1.
- TIFF: the same, 8 or 16 bits.
- Camera RAW (rawler 0.8.0): black and white levels, PPG demosaic (bilinear
  for four-color and X-Trans sensors), the camera's as-shot white balance,
  then the decoder's camera-to-XYZ (D65) matrix to linear sRGB primaries.
  No camera profile, no tone curve: neutral, not matched to the camera's
  JPEG.
- EXIF orientation is applied on decode. Everything below works on the
  oriented image.

## Pipeline

Each output pixel goes through these steps, in this order.

### 1. Geometry

Output position (normalized 0..1 across the cropped result, pixel centers
at `(i + 0.5) / width`) maps to a source position (normalized across the
oriented photo, width `W`, height `H`) by one affine map, the composition of:

1. **Crop**: `p = ((x + u·w)·W', (y + v·h)·H')` in pixels of the turned
   image, where `(W', H')` is `(H, W)` for an odd number of quarter turns,
   else `(W, H)`.
2. **Straighten**: rotate `p` by `−θ` about `(W'/2, H'/2)`, with y down:
   `[cos θ, sin θ; −sin θ, cos θ]`. Positive `θ` turns the picture clockwise.
3. Divide by `(W', H')`.
4. **Flips**, which happen after the turn: `a ↦ 1 − a` (horizontal),
   `b ↦ 1 − b` (vertical).
5. **Quarter turns**, clockwise: one turn takes source `(s, t)` to
   `(1 − t, s)`, so its inverse is `s = b, t = 1 − a`; two turns
   `s = 1 − a, t = 1 − b`; three `s = 1 − b, t = a`.

The source is sampled bilinearly (pixel centers at `+0.5`) in linear light.
Positions outside `0..1` are black. The full-size result is
`round(w·W') × round(h·H')`. A smaller export box-averages the source first
when the result is at least half its size.

### 2. White balance and exposure (linear)

```
t = temperature / 100 · 0.5        g = −tint / 100 · 0.5
gains = (2^t, 2^g, 2^−t) / (0.2126·2^t + 0.7152·2^g + 0.0722·2^−t) · 2^exposure_ev
rgb = rgb · gains
```

The normalization keeps a neutral gray's luminance, so white balance is not
also an exposure control.

### 3. To display

Each channel is clipped to 0..1 and sRGB-encoded.

### 4. Levels (display), master then the channel's own

```
level(x) = out_black + (out_white − out_black) · clamp((x − black) / (white − black), 0, 1)^(1/gamma)
```

### 5. Curves (display), master then the channel's own

Monotone cubic Hermite interpolation (Fritsch–Carlson) through the points:

- secants `d_k = (y_{k+1} − y_k) / (x_{k+1} − x_k)`;
- tangents `m_0 = d_0`, `m_n = d_{n−1}`, and inside
  `m_k = (d_{k−1} + d_k) / 2`, or 0 where the secants change sign;
- for each segment: if `d_k = 0`, `m_k = m_{k+1} = 0`; else with
  `a = m_k / d_k`, `b = m_{k+1} / d_k`, if `a² + b² > 9` scale both by
  `3 / √(a² + b²)`;
- evaluate the cubic Hermite basis on each segment; flat at the end points'
  outputs outside them.

The result is clipped to 0..1.

### 6. Saturation (display)

`y = 0.2126 r + 0.7152 g + 0.0722 b`, then each channel
`clamp(y + (c − y)·(1 + saturation), 0, 1)`.

### 7. Local adjustments, in order

For each mask, with `m` its strength at the pixel's **source** position:

```
D(c) = saturate(encode(decode(c) · 2^exposure_ev), saturation)    per channel, display to display
c = c + (D(c) − c) · m
```

A radial mask, for a photo of `W × H` and long edge `L = max(W, H)`:

```
dx = (u − cx) · W/L          dy = (v − cy) · H/L          (u, v: source position)
x =  dx cos α + dy sin α     y = −dx sin α + dy cos α     (α = angle_deg, clockwise)
d = √((x/rx)² + (y/ry)²)
inner = 1 − max(feather, 0.01)
t = clamp((d − inner) / (1 − inner), 0, 1)
m = 1 − t²(3 − 2t)            then 1 − m if invert
```

Radii are in units of the long edge, so `rx = ry` is a circle on any photo.

## Output

- Display values are written as 8-bit (JPEG, `round(c · 255)`) or 16-bit
  (TIFF, `round(c · 65535)`), with an embedded sRGB ICC profile. The
  orientation is baked in and none is recorded.
- Exports are named `<stem>-edit.<ext>`, then `<stem>-edit (2).<ext>` and so
  on, and never replace an existing file.

## The live preview (informative)

The app's preview evaluates steps 2–6 once per point of a 33³ (17³ while a
control moves) lattice over the proxy's encoding and interpolates
trilinearly on the GPU; each local adjustment's `D` is a second lattice over
display values, blended under the mask computed per pixel. A RAW's proxy is
log-encoded, `p = (log2(v + 2^−12) + 12) / 16`, so the lattice spans 2^−12
to 16 in linear light. This changes nothing above: it is an approximation of
the same functions, measured at a mean ΔE2000 of 0.12–0.23 against direct
evaluation (docs/lanes/5a-photo.md, spike 1).
