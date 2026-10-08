//! Pointer response for a bottom-bar plate.
//!
//! The host feeds one [`Gesture`] per chip: whether the pointer is over it and whether the
//! button is down. [`Gesture::advance`] eases hover and press, and starts a pulse on the press
//! edge. [`Gesture::optics`] turns that into the three quantities the shader spends: a wobble
//! amplitude, a refraction distance, and a glass sheen. All three are bounded, and at rest the
//! wobble and the refraction are exactly zero, so a settled bar does not need another frame.
//!
//! The shader is [`crate::shaders::BAR`]. It draws one rounded glass plate: the sheen sits quiet
//! until hover sweeps it, the plate wobbles and refracts only while the pointer is engaged, and
//! a click shrinks the plate and washes a ring across it.

use crate::geometry::Rect;

/// Bytes of the `Bar` uniform block.
pub const UNIFORM_BYTES: usize = 96;

/// Largest wobble, in logical pixels. A chip should shiver, not swim.
pub const MAX_WOBBLE_PX: f32 = 1.6;
/// Largest backdrop offset, in logical pixels.
pub const MAX_DISTORT_PX: f32 = 5.4;

/// How far a plate is toward hover, press and the click ring.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gesture {
    pub hover: f32,
    pub press: f32,
    pub pulse: f32,
    held: bool,
}

impl Default for Gesture {
    fn default() -> Self {
        Self::rest()
    }
}

impl Gesture {
    #[must_use]
    pub const fn rest() -> Self {
        Self {
            hover: 0.0,
            press: 0.0,
            pulse: 0.0,
            held: false,
        }
    }

    /// A still pose, for a snapshot. `press > 0.5` counts as already held, so the next
    /// [`Self::advance`] does not fire a second pulse.
    #[must_use]
    pub fn pose(hover: f32, press: f32, pulse: f32) -> Self {
        let hover = unit(hover);
        let press = unit(press);
        let pulse = unit(pulse);
        Self {
            hover,
            press,
            pulse,
            held: press > 0.5,
        }
    }

    /// Whether the plate is still changing, including a hover that has arrived and is wobbling.
    #[must_use]
    pub fn live(self) -> bool {
        self.hover > 0.0 || self.press > 0.0 || self.pulse > 0.0
    }

    /// Ease toward `over` / `down` across `dt` seconds. A rising press edge restarts the pulse.
    /// `down` only counts while the pointer is over the plate.
    pub fn advance(&mut self, dt: f32, over: bool, down: bool) {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let down = down && over;
        if down && !self.held {
            self.pulse = 1.0;
        }
        self.held = down;
        self.hover = approach(self.hover, if over { 1.0 } else { 0.0 }, dt, 0.07);
        self.press = approach(self.press, if down { 1.0 } else { 0.0 }, dt, 0.045);
        let decayed = self.pulse * (-dt / 0.16).exp();
        self.pulse = if decayed < 0.004 { 0.0 } else { decayed };
    }

    /// The shader's motion. Rest is a quiet sheen and nothing else.
    #[must_use]
    pub fn optics(self) -> Optics {
        let hover = unit(self.hover);
        let press = unit(self.press);
        Optics {
            wobble_px: (1.15 * hover + 0.45 * press).min(MAX_WOBBLE_PX),
            distort_px: (2.2 * hover + 3.2 * press).min(MAX_DISTORT_PX),
            sheen: 0.22 + 0.58 * hover,
            sheen_gain: 0.18 + 0.62 * hover + 0.20 * press,
        }
    }
}

/// What the plate shader does with a [`Gesture`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Optics {
    pub wobble_px: f32,
    pub distort_px: f32,
    /// Where the sheen sits across the plate, `0` at the upper left and `1` at the lower right.
    pub sheen: f32,
    pub sheen_gain: f32,
}

/// One plate to draw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plate {
    pub rect: Rect,
    pub gesture: Gesture,
    /// Seconds, shared by every plate on the bar so they wobble on one clock.
    pub time: f32,
    /// Phase, so neighbours do not shiver together.
    pub seed: f32,
    pub tint: [f32; 3],
    pub tint_strength: f32,
    pub radius: f32,
    pub bevel: f32,
    pub backdrop_lod: f32,
    pub opacity: f32,
}

impl Plate {
    /// Glass, cool and quiet, at `rect` in `gesture`.
    #[must_use]
    pub fn glass(rect: Rect, gesture: Gesture, time: f32, seed: f32) -> Self {
        Self {
            rect,
            gesture,
            time,
            seed,
            tint: [0.78, 0.88, 1.0],
            tint_strength: 0.16,
            radius: 12.0,
            bevel: 5.0,
            backdrop_lod: 4.5,
            opacity: 1.0,
        }
    }

    /// The `Bar` uniform block for a `target_w × target_h` render target.
    #[must_use]
    pub fn uniform(&self, target_w: f32, target_h: f32) -> [u8; UNIFORM_BYTES] {
        let optics = self.gesture.optics();
        let mut bytes = [0; UNIFORM_BYTES];
        let floats: [[f32; 4]; 6] = [
            self.rect.to_f32(),
            [target_w, target_h, self.time, self.opacity.clamp(0.0, 1.0)],
            [self.tint[0], self.tint[1], self.tint[2], self.tint_strength],
            [
                self.radius.max(0.0),
                self.bevel.max(0.0),
                self.backdrop_lod.max(0.0),
                self.seed,
            ],
            [
                unit(self.gesture.hover),
                unit(self.gesture.press),
                unit(self.gesture.pulse),
                optics.wobble_px,
            ],
            [optics.distort_px, optics.sheen, optics.sheen_gain, 0.0],
        ];
        for (slot, value) in bytes.chunks_exact_mut(4).zip(floats.into_iter().flatten()) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }
}

fn unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn approach(current: f32, target: f32, dt: f32, tau: f32) -> f32 {
    let next = current + (target - current) * (1.0 - (-dt / tau).exp());
    let next = if next.is_finite() { next } else { target };
    if (next - target).abs() < 0.004 {
        target
    } else {
        next.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_is_a_quiet_sheen_with_no_motion() {
        let rest = Gesture::rest();
        assert!(!rest.live());
        let optics = rest.optics();
        assert_eq!(optics.wobble_px.to_bits(), 0.0_f32.to_bits());
        assert_eq!(optics.distort_px.to_bits(), 0.0_f32.to_bits());
        assert!(optics.sheen_gain < 0.25);
        assert!(optics.sheen < 0.3);
    }

    #[test]
    fn hover_eases_in_and_a_press_edge_starts_one_pulse() {
        let mut gesture = Gesture::rest();
        gesture.advance(0.07, true, false);
        assert!((gesture.hover - 0.632).abs() < 0.02, "{}", gesture.hover);
        assert_eq!(gesture.pulse.to_bits(), 0.0_f32.to_bits());
        gesture.advance(0.001, true, true);
        assert!(gesture.pulse > 0.99);
        let pulsed = gesture.pulse;
        gesture.advance(0.001, true, true);
        assert!(
            gesture.pulse < pulsed,
            "holding does not retrigger the ring"
        );
    }

    #[test]
    fn leaving_and_waiting_settle_exactly() {
        let mut gesture = Gesture::pose(1.0, 1.0, 1.0);
        assert!(gesture.live());
        for _ in 0..80 {
            gesture.advance(0.05, false, false);
        }
        assert_eq!(gesture.hover.to_bits(), 0.0_f32.to_bits());
        assert_eq!(gesture.press.to_bits(), 0.0_f32.to_bits());
        assert_eq!(gesture.pulse.to_bits(), 0.0_f32.to_bits());
        assert!(!gesture.live());
    }

    #[test]
    fn a_full_gesture_stays_inside_the_amplitude_bounds() {
        let optics = Gesture::pose(1.0, 1.0, 1.0).optics();
        assert!(optics.wobble_px <= MAX_WOBBLE_PX);
        assert!(optics.distort_px <= MAX_DISTORT_PX);
        assert!(optics.wobble_px > 1.0);
        assert!(optics.distort_px > 4.0);
        assert!(optics.sheen > Gesture::rest().optics().sheen);
        assert!(optics.sheen_gain > 0.9);
    }

    #[test]
    fn the_uniform_carries_the_gesture_and_ignores_non_finite_input() {
        let rect = Rect::new(10, 20, 64, 64);
        let plate = Plate::glass(rect, Gesture::pose(0.5, 0.25, 0.125), 1.5, 2.0);
        let bytes = plate.uniform(1920.0, 1080.0);
        let bits = |offset: usize| {
            f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()).to_bits()
        };
        assert_eq!(bits(0), 10.0_f32.to_bits());
        assert_eq!(bits(8), 64.0_f32.to_bits());
        assert_eq!(bits(16), 1920.0_f32.to_bits());
        assert_eq!(bits(24), 1.5_f32.to_bits());
        assert_eq!(bits(64), 0.5_f32.to_bits());
        assert_eq!(bits(68), 0.25_f32.to_bits());
        assert_eq!(bits(72), 0.125_f32.to_bits());
        assert_eq!(
            bits(76),
            Gesture::pose(0.5, 0.25, 0.0).optics().wobble_px.to_bits()
        );
        let wild = Plate::glass(rect, Gesture::pose(f32::NAN, -2.0, 4.0), 0.0, 0.0);
        let bytes = wild.uniform(100.0, 100.0);
        let bits = |offset: usize| {
            f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()).to_bits()
        };
        assert_eq!(bits(64), 0.0_f32.to_bits());
        assert_eq!(bits(68), 0.0_f32.to_bits());
        assert_eq!(bits(72), 1.0_f32.to_bits());
    }
}
