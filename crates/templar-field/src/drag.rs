//! The drag (and, as an experiment, throw) state machine. Fed raw pointer samples; derives
//! velocity itself, because the toolkit reports positions and nothing more. Projection is
//! closed-form — `d = v²/(2·friction)` along the velocity — never an integrator, so the ghost
//! that is shown before release is exactly where release lands (property-tested).

use crate::tile::TileId;

/// One pointer sample in pixels at a monotonic millisecond time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragSample {
    pub x: f32,
    pub y: f32,
    pub t_ms: u64,
}

/// Tunables. `throw_enabled` is the `shell.home-field.throw` experiment gate; with it off a
/// release lands exactly under the pointer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DragConfig {
    pub throw_enabled: bool,
    /// Below this speed (px/s) a release is a precise placement, never a throw.
    pub throw_min_speed_px_s: f32,
    /// Deceleration the projection assumes, px/s².
    pub friction_px_s2: f32,
    /// A press must travel this far before it becomes a lift; a shorter press is a click.
    pub lift_threshold_px: f32,
    /// How far back velocity smoothing looks.
    pub sample_window_ms: u64,
}

impl Default for DragConfig {
    fn default() -> Self {
        Self {
            throw_enabled: false,
            throw_min_speed_px_s: 600.0,
            friction_px_s2: 3000.0,
            lift_threshold_px: 6.0,
            sample_window_ms: 80,
        }
    }
}

/// Where the pointer is relative to the tile's top-left at the moment of the press, so the tile
/// moves with the pointer rather than jumping to it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GrabOffset {
    pub dx: f32,
    pub dy: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Drag {
    Idle,
    /// Button down on a tile, not yet moved far enough to count as a lift.
    Pressed {
        tile: TileId,
        grab: GrabOffset,
        origin: DragSample,
    },
    /// Lifted and moving. `velocity` is smoothed, px/s; `lifted_at_ms` is when the press became
    /// a lift, so a release can report how long the tile was in the air.
    Lifted {
        tile: TileId,
        grab: GrabOffset,
        pos: DragSample,
        velocity: (f32, f32),
        lifted_at_ms: u64,
    },
}

/// What a sample changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DragEvent {
    Nothing,
    /// The press became a lift: the shell starts drawing the tile raised.
    Lifted,
    /// Still lifted, position changed.
    Moved,
}

/// What a release means.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Release {
    /// A press that never lifted: a click on the tile.
    Click { tile: TileId },
    /// Put the tile down with its top-left at `tile_at` (pointer minus grab offset), after
    /// projection if it was thrown. `lifted_for_ms` is lift-to-release, the throw experiment's
    /// time-to-target.
    Drop {
        tile: TileId,
        tile_at: (f32, f32),
        thrown: bool,
        lifted_for_ms: u64,
    },
}

impl Drag {
    /// Button down over `tile`, with the pointer `grab` into it.
    pub fn press(&mut self, tile: TileId, grab: GrabOffset, sample: DragSample) {
        *self = Drag::Pressed {
            tile,
            grab,
            origin: sample,
        };
    }

    /// Pointer moved.
    pub fn sample(&mut self, sample: DragSample, cfg: &DragConfig) -> DragEvent {
        match *self {
            Drag::Idle => DragEvent::Nothing,
            Drag::Pressed { tile, grab, origin } => {
                let dx = sample.x - origin.x;
                let dy = sample.y - origin.y;
                if dx * dx + dy * dy < cfg.lift_threshold_px * cfg.lift_threshold_px {
                    return DragEvent::Nothing;
                }
                let velocity = instantaneous(origin, sample);
                *self = Drag::Lifted {
                    tile,
                    grab,
                    pos: sample,
                    velocity,
                    lifted_at_ms: sample.t_ms,
                };
                DragEvent::Lifted
            }
            Drag::Lifted {
                tile,
                grab,
                pos,
                velocity,
                lifted_at_ms,
            } => {
                let dt = sample.t_ms.saturating_sub(pos.t_ms);
                let instant = instantaneous(pos, sample);
                // Exponentially weighted: a sample a whole window old replaces the estimate, a
                // sample 1 ms later barely nudges it — one stray jitter cannot fling a tile.
                let alpha = if cfg.sample_window_ms == 0 {
                    1.0
                } else {
                    (dt as f32 / cfg.sample_window_ms as f32).clamp(0.05, 1.0)
                };
                let velocity = (
                    velocity.0 + alpha * (instant.0 - velocity.0),
                    velocity.1 + alpha * (instant.1 - velocity.1),
                );
                *self = Drag::Lifted {
                    tile,
                    grab,
                    pos: sample,
                    velocity,
                    lifted_at_ms,
                };
                DragEvent::Moved
            }
        }
    }

    /// Escape: back to idle, nothing dropped.
    pub fn cancel(&mut self) {
        *self = Drag::Idle;
    }

    pub fn tile(&self) -> Option<TileId> {
        match self {
            Drag::Idle => None,
            Drag::Pressed { tile, .. } | Drag::Lifted { tile, .. } => Some(*tile),
        }
    }

    pub fn is_lifted(&self) -> bool {
        matches!(self, Drag::Lifted { .. })
    }

    /// Where the tile's top-left is right now (lifted only).
    pub fn tile_position(&self) -> Option<(f32, f32)> {
        match self {
            Drag::Lifted { grab, pos, .. } => Some((pos.x - grab.dx, pos.y - grab.dy)),
            _ => None,
        }
    }

    /// Where a release *now* would put the tile's top-left — the ghost's position — within
    /// `bounds` `(x, y, w, h)`. Identical to what [`Drag::release`] returns for the same
    /// state, by construction.
    pub fn projected_position(
        &self,
        cfg: &DragConfig,
        bounds: (f32, f32, f32, f32),
    ) -> Option<(f32, f32)> {
        let Drag::Lifted {
            grab,
            pos,
            velocity,
            ..
        } = self
        else {
            return None;
        };
        let (mut x, mut y) = (pos.x - grab.dx, pos.y - grab.dy);
        if cfg.throw_enabled {
            let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt();
            if speed >= cfg.throw_min_speed_px_s && cfg.friction_px_s2 > 0.0 {
                // d = v² / 2f along v̂  ⇒  v · |v| / 2f.
                let factor = speed / (2.0 * cfg.friction_px_s2);
                x += velocity.0 * factor;
                y += velocity.1 * factor;
            }
        }
        let (bx, by, bw, bh) = bounds;
        Some((x.clamp(bx, bx + bw.max(0.0)), y.clamp(by, by + bh.max(0.0))))
    }

    /// Whether a release now would be a throw rather than a placement.
    pub fn would_throw(&self, cfg: &DragConfig) -> bool {
        match self {
            Drag::Lifted { velocity, .. } => {
                cfg.throw_enabled
                    && (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt()
                        >= cfg.throw_min_speed_px_s
            }
            _ => false,
        }
    }

    /// Button up.
    pub fn release(&mut self, cfg: &DragConfig, bounds: (f32, f32, f32, f32)) -> Option<Release> {
        let result = match *self {
            Drag::Idle => None,
            Drag::Pressed { tile, .. } => Some(Release::Click { tile }),
            Drag::Lifted {
                tile,
                pos,
                lifted_at_ms,
                ..
            } => {
                let thrown = self.would_throw(cfg);
                let tile_at = self.projected_position(cfg, bounds)?;
                Some(Release::Drop {
                    tile,
                    tile_at,
                    thrown,
                    lifted_for_ms: pos.t_ms.saturating_sub(lifted_at_ms),
                })
            }
        };
        *self = Drag::Idle;
        result
    }
}

fn instantaneous(from: DragSample, to: DragSample) -> (f32, f32) {
    let dt = to.t_ms.saturating_sub(from.t_ms).max(1) as f32 / 1000.0;
    ((to.x - from.x) / dt, (to.y - from.y) / dt)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f32, y: f32, t: u64) -> DragSample {
        DragSample { x, y, t_ms: t }
    }

    const BOUNDS: (f32, f32, f32, f32) = (0.0, 0.0, 1000.0, 600.0);

    #[test]
    fn a_short_press_is_a_click_not_a_lift() {
        let cfg = DragConfig::default();
        let mut drag = Drag::Idle;
        drag.press(
            TileId(1),
            GrabOffset { dx: 5.0, dy: 5.0 },
            at(100.0, 100.0, 0),
        );
        assert_eq!(drag.sample(at(102.0, 101.0, 10), &cfg), DragEvent::Nothing);
        assert_eq!(
            drag.release(&cfg, BOUNDS),
            Some(Release::Click { tile: TileId(1) })
        );
        assert_eq!(drag, Drag::Idle);
    }

    #[test]
    fn a_slow_release_lands_under_the_pointer_minus_the_grab() {
        let cfg = DragConfig::default();
        let mut drag = Drag::Idle;
        drag.press(
            TileId(1),
            GrabOffset { dx: 10.0, dy: 4.0 },
            at(100.0, 100.0, 0),
        );
        assert_eq!(drag.sample(at(130.0, 100.0, 50), &cfg), DragEvent::Lifted);
        assert_eq!(drag.sample(at(131.0, 100.0, 1050), &cfg), DragEvent::Moved);
        assert_eq!(drag.tile_position(), Some((121.0, 96.0)));
        assert_eq!(
            drag.release(&cfg, BOUNDS),
            Some(Release::Drop {
                tile: TileId(1),
                tile_at: (121.0, 96.0),
                thrown: false,
                lifted_for_ms: 1000,
            })
        );
    }

    #[test]
    fn a_fast_release_projects_only_when_throwing_is_enabled() {
        let mut drag = Drag::Idle;
        drag.press(
            TileId(1),
            GrabOffset { dx: 0.0, dy: 0.0 },
            at(100.0, 100.0, 0),
        );
        drag.sample(at(200.0, 100.0, 50), &DragConfig::default()); // 2000 px/s
        let off = DragConfig::default();
        assert!(!drag.would_throw(&off));
        assert_eq!(drag.projected_position(&off, BOUNDS), Some((200.0, 100.0)));

        let on = DragConfig {
            throw_enabled: true,
            ..DragConfig::default()
        };
        assert!(drag.would_throw(&on));
        let projected = drag.projected_position(&on, BOUNDS).unwrap();
        // d = 2000² / (2·3000) = 666.7 → clamped to the right edge.
        assert_eq!(projected, (866.6667, 100.0));
        let Some(Release::Drop {
            tile_at, thrown, ..
        }) = drag.release(&on, BOUNDS)
        else {
            panic!("expected a drop");
        };
        assert!(thrown);
        assert_eq!(tile_at, projected, "the ghost is where release lands");
    }

    #[test]
    fn projection_never_leaves_the_bounds() {
        let on = DragConfig {
            throw_enabled: true,
            friction_px_s2: 1.0,
            ..DragConfig::default()
        };
        let mut drag = Drag::Idle;
        drag.press(
            TileId(1),
            GrabOffset { dx: 0.0, dy: 0.0 },
            at(500.0, 300.0, 0),
        );
        drag.sample(at(600.0, 400.0, 10), &on);
        let (x, y) = drag.projected_position(&on, BOUNDS).unwrap();
        assert!((0.0..=1000.0).contains(&x) && (0.0..=600.0).contains(&y));
    }

    #[test]
    fn cancel_returns_to_idle_and_releases_nothing() {
        let cfg = DragConfig::default();
        let mut drag = Drag::Idle;
        drag.press(TileId(1), GrabOffset { dx: 0.0, dy: 0.0 }, at(0.0, 0.0, 0));
        drag.sample(at(50.0, 50.0, 20), &cfg);
        drag.cancel();
        assert_eq!(drag, Drag::Idle);
        assert_eq!(drag.release(&cfg, BOUNDS), None);
    }
}
