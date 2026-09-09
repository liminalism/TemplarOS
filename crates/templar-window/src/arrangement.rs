//! One-press reversible arrangements for the eligible open windows (planning:
//! user-invoked arrangement, never an enforced tiling policy). This module is the pure
//! geometry: [`preset_rects`] turns a preset, a window count (with weights for the cushion)
//! and an output's usable area into deterministic, bounded, non-overlapping rectangles.
//!
//! The stateful half — which windows are eligible, the retained baseline the presets are
//! audition-switched over, and Restore — lives on `WindowService`
//! ([`crate::WindowService::arrange_windows`] / [`crate::WindowService::restore_arrangement`]),
//! because the service is the sole geometry authority. The shell only requests a named preset.

use crate::geometry::Rect;
use serde::{Deserialize, Serialize};

/// A named arrangement of all eligible windows on an output. The first slot is the main one —
/// callers put the activated window first so Focus and MainAndStack feature it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ArrangementPreset {
    /// One window almost full-output; any others share a strip along the bottom.
    Focus,
    /// Two columns of near-equal shares.
    Halves,
    /// Three columns of near-equal shares.
    Thirds,
    /// A near-square grid.
    Quarters,
    /// A large main window on the left, the rest stacked in a right-hand column.
    MainAndStack,
    /// A cushion/treemap-style packing: area follows each window's weight (its baseline area),
    /// packed by weight-balanced binary splits.
    Cushion,
}

impl ArrangementPreset {
    pub const ALL: [ArrangementPreset; 6] = [
        ArrangementPreset::Focus,
        ArrangementPreset::Halves,
        ArrangementPreset::Thirds,
        ArrangementPreset::Quarters,
        ArrangementPreset::MainAndStack,
        ArrangementPreset::Cushion,
    ];

    /// The stable name the protocol and the action manifest use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Focus => "focus",
            Self::Halves => "halves",
            Self::Thirds => "thirds",
            Self::Quarters => "quarters",
            Self::MainAndStack => "main-and-stack",
            Self::Cushion => "cushion",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|preset| preset.name() == name)
    }
}

/// The share of the area's width the main window takes in [`ArrangementPreset::MainAndStack`],
/// in per-mille. Roughly the golden section: large enough to work in, small enough that the
/// stack stays readable.
const MAIN_SHARE_MILLI: i32 = 618;

/// The height of the strip secondary windows share under a [`ArrangementPreset::Focus`]
/// arrangement, in per-mille of the area.
const FOCUS_STRIP_MILLI: i32 = 250;

/// `count` evenly split positions of `extent` starting at `origin`: slot `index` starts at
/// `origin + index * extent / count` — integer arithmetic whose slots tile `extent` exactly,
/// leaving no seam and never overlapping.
fn even_cut(origin: i32, extent: i32, count: i32, index: i32) -> (i32, i32) {
    let start = origin + (extent * index) / count;
    let end = origin + (extent * (index + 1)) / count;
    (start, end - start)
}

/// `count` rects stacked vertically filling `area` evenly.
fn column(area: Rect, count: usize) -> Vec<Rect> {
    let slot_count = i32::try_from(count).unwrap_or(i32::MAX).max(1);
    (0..count)
        .map(|index| {
            let index = i32::try_from(index).unwrap_or(0);
            let (y, height) = even_cut(area.y, area.height, slot_count, index);
            Rect::new(area.x, y, area.width, height)
        })
        .collect()
}

/// `columns` vertical bands of `area`, windows distributed evenly across them in order —
/// column 0 gets the first `ceil(n / columns)` windows, and so on.
fn banded_columns(area: Rect, count: usize, columns: usize) -> Vec<Rect> {
    let columns = columns.clamp(1, count.max(1));
    let per_column = count.div_ceil(columns);
    let columns_needed = count.div_ceil(per_column);
    let band_count = i32::try_from(columns_needed).unwrap_or(1).max(1);
    let mut rects = Vec::with_capacity(count);
    for band in 0..columns_needed {
        let in_band = per_column.min(count - band * per_column);
        let band_index = i32::try_from(band).unwrap_or(0);
        let (x, width) = even_cut(area.x, area.width, band_count, band_index);
        rects.extend(column(Rect::new(x, area.y, width, area.height), in_band));
    }
    rects
}

/// A near-square grid: `rows = ceil(sqrt(n))` rows, filled row by row, the last row absorbing
/// the remainder evenly.
fn grid(area: Rect, count: usize) -> Vec<Rect> {
    let rows = (1..=count)
        .find(|candidate| candidate * candidate >= count)
        .unwrap_or(1);
    let per_row = count.div_ceil(rows);
    let rows_needed = count.div_ceil(per_row);
    let row_count = i32::try_from(rows_needed).unwrap_or(1).max(1);
    let mut rects = Vec::with_capacity(count);
    for row in 0..rows_needed {
        let in_row = per_row.min(count - row * per_row);
        let slot_count = i32::try_from(in_row).unwrap_or(1).max(1);
        let row_index = i32::try_from(row).unwrap_or(0);
        let (y, height) = even_cut(area.y, area.height, row_count, row_index);
        for slot in 0..in_row {
            let slot_index = i32::try_from(slot).unwrap_or(0);
            let (x, width) = even_cut(area.x, area.width, slot_count, slot_index);
            rects.push(Rect::new(x, y, width, height));
        }
    }
    rects
}

/// Weight-balanced binary splits along the longer axis: the cushion pack. Splits happen at the
/// cumulative weight midpoint in window order, so the result is deterministic and every rect
/// tiles `area` exactly.
fn cushion(area: Rect, weights: &[u64], out: &mut Vec<Rect>) {
    match weights.len() {
        0 => {}
        1 => out.push(area),
        _ => {
            let total: u64 = weights.iter().copied().map(|w| w.max(1)).sum();
            let mut cumulative = 0u64;
            let mut split = 1;
            for (index, weight) in weights.iter().enumerate() {
                if cumulative * 2 >= total && index > 0 {
                    break;
                }
                cumulative += (*weight).max(1);
                split = index + 1;
            }
            let split = split.clamp(1, weights.len() - 1);
            let first: u64 = weights[..split].iter().copied().map(|w| w.max(1)).sum();
            let cut_of = |extent: i32| -> i32 {
                let extent_u = u64::from(extent.unsigned_abs());
                let cut = i32::try_from(extent_u * first / total.max(1)).unwrap_or(0);
                cut.clamp(1, (extent - 1).max(1))
            };
            let (a, b) = if area.width >= area.height {
                let cut = cut_of(area.width);
                (
                    Rect::new(area.x, area.y, cut, area.height),
                    Rect::new(area.x + cut, area.y, area.width - cut, area.height),
                )
            } else {
                let cut = cut_of(area.height);
                (
                    Rect::new(area.x, area.y, area.width, cut),
                    Rect::new(area.x, area.y + cut, area.width, area.height - cut),
                )
            };
            cushion(a, &weights[..split], out);
            cushion(b, &weights[split..], out);
        }
    }
}

/// The rectangle every window gets under `preset` in `area`: `weights.len()` windows, in the
/// caller's order (main window first), one rect each in the same order. Deterministic in its
/// inputs; every rect lies inside `area`; no two rects overlap. `weights` only matter to
/// [`ArrangementPreset::Cushion`] — pass each window's baseline area.
pub fn preset_rects(preset: ArrangementPreset, weights: &[u64], area: Rect) -> Vec<Rect> {
    let count = weights.len();
    if count == 0 || area.width < 1 || area.height < 1 {
        return Vec::new();
    }
    if count == 1 {
        return vec![area];
    }
    match preset {
        ArrangementPreset::Focus => {
            if area.height < 2 {
                return banded_columns(area, count, count);
            }
            let strip = ((area.height * FOCUS_STRIP_MILLI) / 1000).clamp(1, area.height - 1);
            let main = Rect::new(area.x, area.y, area.width, area.height - strip);
            let others = banded_columns(
                Rect::new(area.x, area.y + area.height - strip, area.width, strip),
                count - 1,
                count - 1,
            );
            std::iter::once(main).chain(others).collect()
        }
        ArrangementPreset::Halves => banded_columns(area, count, 2),
        ArrangementPreset::Thirds => banded_columns(area, count, 3),
        ArrangementPreset::Quarters => grid(area, count),
        ArrangementPreset::MainAndStack => {
            if area.width < 2 {
                return grid(area, count);
            }
            let main_width = ((area.width * MAIN_SHARE_MILLI) / 1000).clamp(1, area.width - 1);
            let main = Rect::new(area.x, area.y, main_width, area.height);
            let stack = column(
                Rect::new(
                    area.x + main_width,
                    area.y,
                    area.width - main_width,
                    area.height,
                ),
                count - 1,
            );
            std::iter::once(main).chain(stack).collect()
        }
        ArrangementPreset::Cushion => {
            let mut rects = Vec::with_capacity(count);
            cushion(area, weights, &mut rects);
            rects
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Output, is_recoverable};

    /// `preset-geometry`: every preset, for one through twelve windows, on plain, offset and
    /// mixed-scale outputs — deterministic, inside the area, pairwise disjoint, and every rect
    /// recoverable on that output.
    #[test]
    fn arrangement_geometry_is_deterministic_bounded_disjoint_and_recoverable() {
        let outputs = [
            Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("DP-1", Rect::new(1920, 0, 3840, 2160), 2000),
            Output::new("HDMI-A-1", Rect::new(-1280, -1024, 1280, 1024), 1500),
        ];
        for output in &outputs {
            let area = output.usable();
            for preset in ArrangementPreset::ALL {
                for count in 1..=12usize {
                    let weights: Vec<u64> =
                        (0..count).map(|index| (index as u64 % 5) * 7 + 1).collect();
                    let first = preset_rects(preset, &weights, area);
                    let second = preset_rects(preset, &weights, area);
                    assert_eq!(first, second, "{preset:?} n={count} is deterministic");
                    assert_eq!(first.len(), count, "{preset:?} n={count}: one rect each");
                    for rect in &first {
                        assert!(rect.width >= 1 && rect.height >= 1, "{preset:?} n={count}");
                        assert!(
                            rect.x >= area.x
                                && rect.y >= area.y
                                && rect.x + rect.width <= area.x + area.width
                                && rect.y + rect.height <= area.y + area.height,
                            "{preset:?} n={count}: {rect:?} outside {area:?}"
                        );
                        assert!(
                            is_recoverable(*rect, std::slice::from_ref(output)),
                            "{preset:?} n={count}: {rect:?} unrecoverable on {}",
                            output.name
                        );
                    }
                    for (index, rect) in first.iter().enumerate() {
                        for other in &first[index + 1..] {
                            assert!(
                                rect.intersection(*other).is_none(),
                                "{preset:?} n={count}: {rect:?} overlaps {other:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The preset names are stable round-trips — they are what the protocol and the action
    /// manifest carry.
    #[test]
    fn arrangement_preset_names_round_trip() {
        for preset in ArrangementPreset::ALL {
            assert_eq!(ArrangementPreset::from_name(preset.name()), Some(preset));
        }
        assert_eq!(ArrangementPreset::from_name("cascade"), None);
    }
}
