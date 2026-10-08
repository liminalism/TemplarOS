//! One rasterized body per Place tile, rebuilt only when what it depends on changes.
//!
//! A Place's body is the most expensive thing on Home to draw — layout, anti-aliased shapes,
//! shading, grain — and it changes only when the daemon reports a different tree, the tile is
//! resized, the output's scale moves or the theme is switched. Everything else about a frame
//! (hover, a bloom's mark, the names) is drawn live over the bitmap, so a repaint is a blit.

use std::collections::HashMap;
use std::path::PathBuf;

use pixelkit_raster::Bitmap;
use templar_field::TileId;
use templar_treemap::garden::GardenLayout;

/// Everything a body's pixels depend on. Two equal keys draw identical bitmaps.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BodyKey {
    pub root: PathBuf,
    /// [`super::fingerprint`] of the tree as served, and the budget it was cut to.
    pub tree: u64,
    /// The body's size in physical pixels.
    pub size: (i32, i32),
    /// Physical pixels per logical one, as bits so the key is exact.
    pub scale: u32,
    pub theme: &'static str,
    pub palette: templar_types::appearance::Palette,
    pub form: String,
    pub seed: u64,
}

/// A body as drawn: its geometry (for hit-testing, names and the next relayout) and its pixels.
#[derive(Debug)]
pub(super) struct Body {
    pub key: BodyKey,
    pub layout: GardenLayout,
    /// Each layout item's path relative to the place's root, in `layout.items` order.
    pub paths: Vec<String>,
    pub names: Vec<String>,
    pub bitmap: Bitmap,
    used: u64,
}

/// What a build produces.
pub(super) struct Built {
    pub layout: GardenLayout,
    pub paths: Vec<String>,
    pub names: Vec<String>,
    pub bitmap: Bitmap,
}

/// How many bodies are kept. A Home shows a handful of grown places; the rest of the ledger's
/// are never on screen together, and a body is a few hundred kilobytes.
const KEPT: usize = 24;

#[derive(Debug, Default)]
pub(super) struct BodyCache {
    bodies: HashMap<TileId, Body>,
    clock: u64,
    /// How many times a body has been built — what the tests observe.
    #[cfg(test)]
    pub builds: usize,
}

impl BodyCache {
    /// Borrow a matching body without allocating a replacement key on every repaint.
    pub(super) fn matching(
        &mut self,
        tile: TileId,
        matches: impl FnOnce(&BodyKey) -> bool,
    ) -> Option<&Body> {
        let body = self.bodies.get_mut(&tile)?;
        if !matches(&body.key) {
            return None;
        }
        self.clock += 1;
        body.used = self.clock;
        Some(body)
    }
    /// The body for `tile` at `key`: the cached one when its key is `key`, otherwise the result
    /// of `build`, which is handed the tile's previous layout (when there is one) so a relayout
    /// can keep what did not move where it was.
    pub(super) fn get_or_build(
        &mut self,
        tile: TileId,
        key: BodyKey,
        build: impl FnOnce(Option<&GardenLayout>) -> Built,
    ) -> &Body {
        self.clock += 1;
        let clock = self.clock;
        let fresh = self.bodies.get(&tile).is_some_and(|body| body.key == key);
        if !fresh {
            let fresh_body = build(self.bodies.get(&tile).map(|body| &body.layout));
            #[cfg(test)]
            {
                self.builds += 1;
            }
            self.bodies.insert(
                tile,
                Body {
                    key,
                    layout: fresh_body.layout,
                    paths: fresh_body.paths,
                    names: fresh_body.names,
                    bitmap: fresh_body.bitmap,
                    used: clock,
                },
            );
            self.evict();
        }
        let body = self.bodies.get_mut(&tile).expect("present or just built");
        body.used = clock;
        body
    }

    /// Past [`KEPT`], drop the bodies that have gone longest without being drawn.
    fn evict(&mut self) {
        while self.bodies.len() > KEPT {
            let Some(oldest) = self
                .bodies
                .iter()
                .min_by_key(|(_, body)| body.used)
                .map(|(tile, _)| *tile)
            else {
                return;
            };
            self.bodies.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use templar_treemap::garden::{Bounds, LayoutKind};

    fn key(size: (i32, i32), tree: u64) -> BodyKey {
        BodyKey {
            root: PathBuf::from("/home/dk/work"),
            tree,
            size,
            scale: 1.0_f32.to_bits(),
            theme: "mineral",
            palette: templar_types::appearance::LIGHT,
            form: "branch".to_owned(),
            seed: 7,
        }
    }

    fn built(width: f32) -> Built {
        Built {
            layout: GardenLayout {
                kind: LayoutKind::Strip,
                bounds: Bounds {
                    x: 0.0,
                    y: 0.0,
                    w: width,
                    h: 10.0,
                },
                items: Vec::new(),
            },
            paths: Vec::new(),
            names: Vec::new(),
            bitmap: Bitmap::new(1, 1),
        }
    }

    #[test]
    fn a_body_is_rebuilt_when_its_key_changes_and_never_otherwise() {
        let mut cache = BodyCache::default();
        let tile = TileId(1);
        cache.get_or_build(tile, key((100, 80), 1), |previous| {
            assert!(previous.is_none(), "nothing drawn here before");
            built(100.0)
        });
        for _ in 0..3 {
            cache.get_or_build(tile, key((100, 80), 1), |_| {
                panic!("the key has not changed")
            });
        }
        assert_eq!(cache.builds, 1);

        // A resize is a new key; the build is handed what was there, for a stable relayout.
        cache.get_or_build(tile, key((140, 80), 1), |previous| {
            assert_eq!(previous.map(|layout| layout.bounds.w), Some(100.0));
            built(140.0)
        });
        // So is a different tree at the same size.
        cache.get_or_build(tile, key((140, 80), 2), |_| built(140.0));
        assert_eq!(cache.builds, 3);

        // Another tile is its own body, and does not disturb this one's.
        cache.get_or_build(TileId(2), key((140, 80), 2), |previous| {
            assert!(previous.is_none());
            built(140.0)
        });
        cache.get_or_build(tile, key((140, 80), 2), |_| panic!("still cached"));
        assert_eq!(cache.builds, 4);
    }
    #[test]
    fn live_palette_change_rebuilds_pixels_but_preserves_previous_geometry() {
        let mut cache = BodyCache::default();
        let tile = TileId(1);
        let light = key((100, 80), 1);
        cache.get_or_build(tile, light.clone(), |_| built(100.0));
        let mut dark = light;
        dark.palette = templar_types::appearance::DARK;
        cache.get_or_build(tile, dark.clone(), |previous| {
            assert_eq!(previous.map(|layout| layout.bounds.w), Some(100.0));
            built(100.0)
        });
        assert_eq!(cache.builds, 2);
        cache.get_or_build(tile, dark, |_| panic!("unchanged appearance stays cached"));
        assert_eq!(cache.builds, 2);
    }
}
