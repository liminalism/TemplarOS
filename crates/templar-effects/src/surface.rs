//! Surface materials for the few places that wear one: notification cards, the login screen,
//! Home's ground. One shader (`shaders/surface.wgsl`), one uniform block, a material switch.
//!
//! Noise is the shared tile (four smooth octaves per fetch); only the stained glass's Voronoi
//! sites hash per cell, since neighbouring texels of a smooth tile are correlated.
//!
//! Every material is a rounded rectangle with an analytic anti-aliased edge and a bevel whose
//! normal the light reads, so a panel has a physical rim whatever it is made of. The glass
//! family reads a backdrop the host supplies already blurred into a mip chain (`shape.z` picks
//! the level); the rest are opaque and read only the shared noise texture.
//!
//! Surfaces are meant to be *retained*: a panel that has not changed size, material or light
//! is not redrawn — the host keeps the pixels. [`Material::animated`] says which ones change
//! with time at all (only the metal's sheen, and only while it is being shown off).

use crate::geometry::Rect;

/// Bytes of the `Surface` uniform block.
pub const UNIFORM_BYTES: usize = 112;

/// What a surface is made of. The discriminant is the shader's material id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum Material {
    /// Clear glass: the blurred backdrop, a faint tint, a refracting bevel and a fresnel rim.
    Glass = 0,
    /// Glass frosted in an etched pattern: clear lines through a frosted field.
    EtchedGlass = 1,
    /// Leaded panes of coloured glass, each a Voronoi cell, lit through.
    StainedGlass = 2,
    /// Matte stone: layered grain, speckle, crevices.
    Stone = 3,
    /// Polished stone: veined, with a clear-coat highlight.
    GlassyStone = 4,
    /// Brushed metal with an anisotropic sheen that follows the light.
    MetalSheen = 5,
    /// Rough, bumpy wood: rings, grain and pores. Home's ground.
    RoughWood = 6,
}

impl Material {
    pub const ALL: [Material; 7] = [
        Material::Glass,
        Material::EtchedGlass,
        Material::StainedGlass,
        Material::Stone,
        Material::GlassyStone,
        Material::MetalSheen,
        Material::RoughWood,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Material::Glass => "glass",
            Material::EtchedGlass => "etched glass",
            Material::StainedGlass => "stained glass",
            Material::Stone => "stone",
            Material::GlassyStone => "glassy stone",
            Material::MetalSheen => "metal sheen",
            Material::RoughWood => "rough wood",
        }
    }

    /// Whether the material shows the backdrop through itself.
    #[must_use]
    pub fn needs_backdrop(self) -> bool {
        matches!(
            self,
            Material::Glass | Material::EtchedGlass | Material::StainedGlass
        )
    }

    /// Whether the material changes with time (and so is ever worth redrawing unchanged).
    #[must_use]
    pub fn animated(self) -> bool {
        matches!(self, Material::MetalSheen)
    }

    #[must_use]
    pub fn next(self) -> Material {
        let index = Material::ALL.iter().position(|&m| m == self).unwrap_or(0);
        Material::ALL[(index + 1) % Material::ALL.len()]
    }
}

/// One surface to draw: where, what of, and how lit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub rect: Rect,
    pub material: Material,
    /// Corner radius and bevel width, logical pixels.
    pub radius: f32,
    pub bevel: f32,
    /// Linear RGB tint and how strongly it applies.
    pub tint: [f32; 3],
    pub tint_strength: f32,
    /// Light direction (x right, y down, z toward the viewer; normalized by the shader) and
    /// intensity.
    pub light: [f32; 3],
    pub light_intensity: f32,
    /// Logical pixels per pattern unit: larger is coarser.
    pub scale: f32,
    pub roughness: f32,
    pub seed: f32,
    /// Material detail, `0..=1` (etch density, lead width, vein strength, grain depth).
    pub detail: f32,
    /// Backdrop mip level the glass family reads: the blur.
    pub backdrop_lod: f32,
    pub opacity: f32,
    /// Seconds, for [`Material::animated`] materials.
    pub time: f32,
}

impl Surface {
    /// A tasteful starting point for `material` at `rect`.
    #[must_use]
    pub fn preset(material: Material, rect: Rect) -> Self {
        let base = Surface {
            rect,
            material,
            radius: 14.0,
            bevel: 6.0,
            tint: [1.0, 1.0, 1.0],
            tint_strength: 0.0,
            light: [-0.45, -0.6, 0.66],
            light_intensity: 1.0,
            scale: 180.0,
            roughness: 0.5,
            seed: 0.0,
            detail: 0.5,
            backdrop_lod: 4.0,
            opacity: 1.0,
            time: 0.0,
        };
        match material {
            Material::Glass => Surface {
                tint: [0.80, 0.90, 1.0],
                tint_strength: 0.12,
                roughness: 0.08,
                backdrop_lod: 4.5,
                ..base
            },
            Material::EtchedGlass => Surface {
                tint: [0.92, 0.96, 1.0],
                tint_strength: 0.18,
                scale: 90.0,
                roughness: 0.3,
                detail: 0.55,
                backdrop_lod: 5.0,
                ..base
            },
            Material::StainedGlass => Surface {
                scale: 70.0,
                roughness: 0.2,
                detail: 0.5,
                backdrop_lod: 3.0,
                ..base
            },
            Material::Stone => Surface {
                tint: [0.56, 0.54, 0.50],
                tint_strength: 1.0,
                scale: 220.0,
                roughness: 0.85,
                detail: 0.6,
                ..base
            },
            Material::GlassyStone => Surface {
                tint: [0.20, 0.24, 0.27],
                tint_strength: 1.0,
                scale: 260.0,
                roughness: 0.12,
                detail: 0.7,
                ..base
            },
            Material::MetalSheen => Surface {
                tint: [0.74, 0.75, 0.78],
                tint_strength: 1.0,
                scale: 400.0,
                roughness: 0.3,
                detail: 0.6,
                ..base
            },
            Material::RoughWood => Surface {
                tint: [0.46, 0.30, 0.17],
                tint_strength: 1.0,
                radius: 0.0,
                bevel: 0.0,
                scale: 320.0,
                roughness: 0.75,
                detail: 0.7,
                ..base
            },
        }
    }

    /// The `Surface` uniform block for a `target_w × target_h` render target.
    #[must_use]
    pub fn uniform(&self, target_w: f32, target_h: f32) -> [u8; UNIFORM_BYTES] {
        let mut bytes = [0; UNIFORM_BYTES];
        let floats: [[f32; 4]; 6] = [
            self.rect.to_f32(),
            [target_w, target_h, self.time, self.opacity.clamp(0.0, 1.0)],
            [self.tint[0], self.tint[1], self.tint[2], self.tint_strength],
            [
                self.light[0],
                self.light[1],
                self.light[2],
                self.light_intensity,
            ],
            [
                self.radius.max(0.0),
                self.bevel.max(0.0),
                self.backdrop_lod.max(0.0),
                0.0,
            ],
            [
                self.scale.max(1.0),
                self.roughness.clamp(0.02, 1.0),
                self.seed,
                self.detail.clamp(0.0, 1.0),
            ],
        ];
        for (slot, value) in bytes.chunks_exact_mut(4).zip(floats.iter().flatten()) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
        bytes[96..100].copy_from_slice(&(self.material as u32).to_le_bytes());
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_material_has_a_preset_and_a_distinct_id() {
        let rect = Rect::new(0, 0, 300, 120);
        let mut ids: Vec<u32> = Material::ALL.iter().map(|&m| m as u32).collect();
        ids.dedup();
        assert_eq!(ids.len(), Material::ALL.len());
        for material in Material::ALL {
            let bytes = Surface::preset(material, rect).uniform(1920.0, 1080.0);
            assert_eq!(
                u32::from_le_bytes(bytes[96..100].try_into().unwrap()),
                material as u32
            );
        }
    }

    #[test]
    fn next_cycles_through_all() {
        let mut m = Material::Glass;
        for _ in 0..Material::ALL.len() {
            m = m.next();
        }
        assert_eq!(m, Material::Glass);
    }
}
