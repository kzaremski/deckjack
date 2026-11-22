//! 3D LUTs: the pipeline baked into a lattice the frame's GPU interpolates.
//!
//! A LUT of size `n` holds `n^3` RGB entries. Entry `(r, g, b)` is the
//! pipeline evaluated at input `(r, g, b) / (n - 1)`, stored red-fastest:
//! index `(b * n + g) * n + r`. That is the memory order WebGL2's
//! `texImage3D` expects with x = red, y = green, z = blue, so the frame
//! uploads the bytes as they come. Values are IEEE half floats, which an
//! `RGB16F` texture filters natively in WebGL2.

use half::f16;

/// While a slider is moving: 17^3 entries, 29 KiB.
pub const DRAG: usize = 17;
/// When it lets go: 33^3 entries, 216 KiB.
pub const SETTLED: usize = 33;

/// A baked LUT, as the frame receives it.
#[derive(Debug, Clone)]
pub struct Lut {
    pub size: usize,
    /// `size^3 * 3` half floats, red-fastest.
    pub data: Vec<u16>,
}

impl Lut {
    /// Evaluate `f` at every lattice point. `f` takes and returns 0..1.
    pub fn bake(size: usize, f: impl Fn([f32; 3]) -> [f32; 3]) -> Lut {
        let size = size.clamp(2, 65);
        let step = 1.0 / (size - 1) as f32;
        let mut data = Vec::with_capacity(size * size * size * 3);
        for b in 0..size {
            for g in 0..size {
                for r in 0..size {
                    let out = f([r as f32 * step, g as f32 * step, b as f32 * step]);
                    data.extend(out.iter().map(|v| f16::from_f32(*v).to_bits()));
                }
            }
        }
        Lut { size, data }
    }

    /// Little-endian bytes, for base64 on the wire.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.data.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn at(&self, r: usize, g: usize, b: usize) -> [f32; 3] {
        let i = ((b * self.size + g) * self.size + r) * 3;
        [
            f16::from_bits(self.data[i]).to_f32(),
            f16::from_bits(self.data[i + 1]).to_f32(),
            f16::from_bits(self.data[i + 2]).to_f32(),
        ]
    }

    /// Trilinear lookup, as the GPU's `texture()` on a `LINEAR`-filtered 3D
    /// texture does it. Used to describe what the preview shows (histogram
    /// in `photo.describe`) without the GPU.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let n = self.size - 1;
        let pos = rgb.map(|c| c.clamp(0.0, 1.0) * n as f32);
        let i0 = pos.map(|p| (p.floor() as usize).min(n - 1));
        let t = [
            pos[0] - i0[0] as f32,
            pos[1] - i0[1] as f32,
            pos[2] - i0[2] as f32,
        ];
        let mut out = [0.0f32; 3];
        for (corner, weight) in (0..8).map(|k| {
            let (dr, dg, db) = (k & 1, (k >> 1) & 1, (k >> 2) & 1);
            let w = (if dr == 1 { t[0] } else { 1.0 - t[0] })
                * (if dg == 1 { t[1] } else { 1.0 - t[1] })
                * (if db == 1 { t[2] } else { 1.0 - t[2] });
            (self.at(i0[0] + dr, i0[1] + dg, i0[2] + db), w)
        }) {
            for c in 0..3 {
                out[c] += corner[c] * weight;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_are_red_fastest_at_lattice_points() {
        // The frame uploads these bytes as a texture with x = red. If the
        // order were blue-fastest, red and blue would swap on screen.
        let lut = Lut::bake(3, |c| c);
        assert_eq!(lut.data.len(), 27 * 3);
        // Index 1 is (r=1, g=0, b=0): input (0.5, 0, 0).
        assert_eq!(f16::from_bits(lut.data[3]).to_f32(), 0.5);
        assert_eq!(f16::from_bits(lut.data[4]).to_f32(), 0.0);
        // Index 9 is (r=0, g=0, b=1).
        assert_eq!(f16::from_bits(lut.data[9 * 3 + 2]).to_f32(), 0.5);
        assert_eq!(lut.to_bytes().len(), 27 * 3 * 2);
        assert_eq!(lut.to_bytes()[6..8], lut.data[3].to_le_bytes());
    }

    #[test]
    fn trilinear_lookup_reproduces_a_linear_function_between_points() {
        let lut = Lut::bake(5, |[r, g, b]| [0.5 * r + 0.25, g * b, 1.0 - b]);
        let out = lut.apply([0.3, 0.0, 0.6]);
        assert!((out[0] - 0.4).abs() < 1e-3, "{out:?}");
        assert!((out[2] - 0.4).abs() < 1e-3, "{out:?}");
        // The top edge is a lattice point, not past the end.
        let top = lut.apply([1.0, 1.0, 1.0]);
        assert!((top[1] - 1.0).abs() < 1e-3, "{top:?}");
    }
}
