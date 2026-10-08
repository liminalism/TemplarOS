//! The one noise texture every effect samples.
//!
//! Procedural shaders usually hash four lattice corners per pixel per octave. The Bend staged-
//! effects experiments (`Bend-experiment/vulkan-port/FOUNDATION.md`) measured the alternative
//! that transfers: evaluate the lattice once and *reuse* it. Here that is a small tileable
//! RGBA8 texture of already-smooth value noise, so the GPU's bilinear filter does the
//! interpolation and an octave costs one texture fetch. The same experiments found a
//! *full-resolution* cached basis slower than recomputing, which is why this is 128² and
//! repeats, not screen-sized.
//!
//! Each channel is an independent noise at its own lattice period across the tile — R coarse
//! to A fine — so one fetch yields four octaves.

/// Edge of the noise texture in texels.
pub const NOISE_SIZE: u32 = 128;
/// Lattice periods per tile for R, G, B, A.
pub const PERIODS: [u32; 4] = [4, 8, 16, 32];

/// The noise texture, `size × size` RGBA8 texels, row-major. Tileable in both directions
/// (each period divides `size`), deterministic, and smooth to the first derivative
/// (quintic interpolation), which bump mapping needs.
#[must_use]
pub fn noise_rgba(size: u32) -> Vec<u8> {
    let size = size.max(PERIODS[3]);
    let mut out = vec![0; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let at = ((y * size + x) * 4) as usize;
            for (channel, &period) in PERIODS.iter().enumerate() {
                let value = value_noise(x, y, size, period, channel as u32);
                out[at + channel] = (value * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

fn value_noise(x: u32, y: u32, size: u32, period: u32, seed: u32) -> f32 {
    let scale = period as f32 / size as f32;
    let fx = x as f32 * scale;
    let fy = y as f32 * scale;
    let (ix, iy) = (fx.floor() as u32, fy.floor() as u32);
    let (tx, ty) = (quintic(fx.fract()), quintic(fy.fract()));
    let corner = |dx: u32, dy: u32| lattice((ix + dx) % period, (iy + dy) % period, seed);
    let top = lerp(corner(0, 0), corner(1, 0), tx);
    let bottom = lerp(corner(0, 1), corner(1, 1), tx);
    lerp(top, bottom, ty)
}

fn quintic(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn lattice(x: u32, y: u32, seed: u32) -> f32 {
    let mut h =
        x.wrapping_mul(0x8DA6_B343) ^ y.wrapping_mul(0xD816_3841) ^ seed.wrapping_mul(0xCB1A_B31F);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h >> 8) as f32 / 16_777_215.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_sized() {
        let a = noise_rgba(NOISE_SIZE);
        assert_eq!(a.len(), (NOISE_SIZE * NOISE_SIZE * 4) as usize);
        assert_eq!(a, noise_rgba(NOISE_SIZE));
    }

    #[test]
    fn tiles_without_a_seam() {
        // The step across the wrap edge is no larger than the largest step inside the tile.
        let size = NOISE_SIZE as usize;
        let tex = noise_rgba(NOISE_SIZE);
        for channel in 0..4 {
            let at = |x: usize, y: usize| i32::from(tex[(y * size + x) * 4 + channel]);
            let mut inner = 0;
            for y in 0..size {
                for x in 0..size - 1 {
                    inner = inner.max((at(x + 1, y) - at(x, y)).abs());
                }
            }
            for y in 0..size {
                assert!(
                    (at(0, y) - at(size - 1, y)).abs() <= inner,
                    "channel {channel} seams at row {y}"
                );
            }
            for x in 0..size {
                assert!(
                    (at(x, 0) - at(x, size - 1)).abs() <= inner,
                    "channel {channel} seams at column {x}"
                );
            }
        }
    }

    #[test]
    fn channels_are_independent_and_spread() {
        let tex = noise_rgba(NOISE_SIZE);
        for channel in 0..4 {
            let values: Vec<u8> = tex.iter().skip(channel).step_by(4).copied().collect();
            let (min, max) = (values.iter().min().unwrap(), values.iter().max().unwrap());
            assert!(max - min > 120, "channel {channel} spans only {min}..{max}");
        }
        assert_ne!(tex[0], tex[1]);
    }
}
