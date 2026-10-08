//! Where Home's live windows sit on the field: a pure answer over cells, with no pixels, no `Ui`
//! and no I/O, so every rule below is pinned by a unit test rather than by a screenshot.
//!
//! Live work (terminals, folders, images and every other window) is presentation, not document:
//! its cards are drawn in grid cells no persistent tile occupies, and nothing here ever edits
//! the `templar_field` document. The rules, in the order they bite:
//!
//! - **Clusters.** Windows that share a folder form one cluster; windows with no folder form one
//!   cluster of unfiled tools (terminals, file managers, image viewers) and one of everything
//!   else. Clusters are ordered by their most recently used member, members by recency.
//! - **Stability.** A card keeps its cells for as long as its window exists, its cluster is the
//!   same, and nothing persistent has been put on top of it. Activation, recency and parking
//!   never move a card: a parked window's card stays exactly where it was, which is what "it
//!   minimises to that area" means on Home.
//! - **Centre outward.** A card that needs cells goes to the free position nearest its cluster's
//!   centroid, or the grid's centre for a cluster with no cards yet, keeping a one-cell gap from
//!   other clusters whenever that is possible.
//! - **Overflow.** Windows that fit nowhere are counted on one overflow card, which takes the
//!   cells of the least recently used card placed in this pass (else the least recent kept one).
//! - **By hand.** A card the person dragged keeps the cells it was dropped on ([`Arrangement::
//!   move_card`]). The drop is remembered as a [`Pin`]: a soft hint, not a field tile, honoured
//!   when the card next needs cells on the same grid and those cells are still free, and
//!   otherwise ignored in favour of the rules above.

use std::collections::BTreeMap;
use std::path::PathBuf;

use templar_field::{Cell, Placement, Span};
use templar_window::WindowId;

/// A card's preferred size, clamped to the grid.
pub const CARD: Span = Span::new(3, 2);
/// The fallback when a full card cannot fit alongside its cluster.
pub const SMALL_CARD: Span = Span::new(2, 1);

/// Which cluster a window belongs to. Derived `Ord` is the deterministic tie-break between
/// clusters whose most recent members are equally recent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ClusterKey {
    /// Every window whose folder is this one.
    Folder(PathBuf),
    /// Terminals, file managers and image viewers with no folder.
    UnfiledTools,
    /// Every other window with no folder.
    UnfiledOther,
}

/// One live window as the layout sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkWindow {
    pub window: WindowId,
    pub cluster: ClusterKey,
    /// `ShellToplevel::mru_rank`: 0 is the most recently used.
    pub mru: u32,
}

/// One window's card and the cells it occupies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub window: WindowId,
    pub cluster: ClusterKey,
    pub placement: Placement,
}

/// The whole answer for one grid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Arrangement {
    /// The grid this was arranged on. A different grid is arranged from scratch.
    pub grid: (u16, u16),
    /// Kept slots first, in their previous order, then this pass's new ones in placement order.
    pub slots: Vec<Slot>,
    /// Windows with no card of their own, most recent first.
    pub overflow: Vec<WindowId>,
    /// Where the "+N windows" card sits, if there is overflow and any room at all.
    pub overflow_card: Option<Placement>,
}

impl Arrangement {
    pub fn slot(&self, window: WindowId) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.window == window)
    }

    /// Whether `window`'s card could sit on `to`: on the grid, and clear of `occupied` (the
    /// persistent tiles), every other card and the overflow card. Its own current cells do not
    /// count against it, so a card can be nudged by a single cell.
    pub fn can_move(&self, window: WindowId, to: Placement, occupied: &[Placement]) -> bool {
        if self.slot(window).is_none() {
            return false;
        }
        let mut board = Board::new(self.grid.0, self.grid.1, occupied);
        for slot in self.slots.iter().filter(|slot| slot.window != window) {
            board.mark(slot.placement, Use::Fixed);
        }
        if let Some(card) = self.overflow_card {
            board.mark(card, Use::Fixed);
        }
        board.is_free(to)
    }

    /// Put `window`'s card on `to` if [`Self::can_move`] allows it, leaving every other card
    /// where it is. Returns whether the card moved; a refused or unchanged move changes nothing.
    pub fn move_card(&mut self, window: WindowId, to: Placement, occupied: &[Placement]) -> bool {
        if !self.can_move(window, to, occupied) {
            return false;
        }
        match self.slots.iter_mut().find(|slot| slot.window == window) {
            Some(slot) if slot.placement != to => {
                slot.placement = to;
                true
            }
            _ => false,
        }
    }
}

/// Where the person dropped a card, and on which grid. Only ever a hint: [`arrange_with_hints`]
/// uses it for a card that needs cells, on that grid, when every one of them is still free.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    pub grid: (u16, u16),
    pub placement: Placement,
}

/// Hand placements by window.
pub type Pins = BTreeMap<WindowId, Pin>;

/// Arrange `windows` on a `cols`×`rows` grid whose `occupied` placements belong to persistent
/// tiles, keeping every slot of `previous` that is still valid. Pass `Arrangement::default()` as
/// `previous` to arrange from scratch (the caller does so when the container changes); a change
/// of grid size, or a previous arrangement with no slots, does so by itself.
pub fn arrange(
    previous: &Arrangement,
    cols: u16,
    rows: u16,
    occupied: &[Placement],
    windows: &[WorkWindow],
) -> Arrangement {
    arrange_with_hints(
        previous,
        cols,
        rows,
        occupied,
        windows,
        &crate::work_positions::WorkPositions::default(),
        &Pins::new(),
    )
}

/// [`arrange`], with the soft memory: per-folder centroids for a cluster that has no cards yet,
/// and hand placements for a card whose previous slot is gone. A kept slot always wins over
/// both, so neither can ever move a card that is already on the grid.
pub fn arrange_with_hints(
    previous: &Arrangement,
    cols: u16,
    rows: u16,
    occupied: &[Placement],
    windows: &[WorkWindow],
    hints: &crate::work_positions::WorkPositions,
    pins: &Pins,
) -> Arrangement {
    let mut board = Board::new(cols, rows, occupied);
    let clusters = clusters(windows);
    let cluster_index = |key: &ClusterKey| {
        clusters
            .iter()
            .position(|(cluster, _)| cluster == key)
            .unwrap_or(0)
    };
    let mru_of = |window: WindowId| {
        windows
            .iter()
            .find(|candidate| candidate.window == window)
            .map_or(u32::MAX, |candidate| candidate.mru)
    };

    let mut slots: Vec<Slot> = Vec::with_capacity(windows.len());
    if previous.grid == (cols, rows) {
        for slot in &previous.slots {
            let still = windows
                .iter()
                .any(|window| window.window == slot.window && window.cluster == slot.cluster);
            if still && board.is_free(slot.placement) {
                board.mark(slot.placement, Use::Card(cluster_index(&slot.cluster)));
                slots.push(slot.clone());
            }
        }
    }
    place_pins(
        &mut board,
        &mut slots,
        windows,
        pins,
        (cols, rows),
        cluster_index,
    );
    let kept = slots.len();

    let preferred = clamp(CARD, cols, rows);
    let small = clamp(SMALL_CARD, cols, rows);
    let mut overflow = Vec::new();
    for (index, (key, members)) in clusters.iter().enumerate() {
        for window in members {
            if slots.iter().any(|slot| slot.window == window.window) {
                continue;
            }
            let remembered = match key {
                ClusterKey::Folder(folder) => hints.target(folder, cols, rows),
                _ => None,
            };
            let target = centroid(&slots, key).or(remembered).unwrap_or((
                i64::from(cols),
                i64::from(rows),
                1,
            ));
            let spans: &[Span] = if small == preferred {
                &[preferred]
            } else {
                &[preferred, small]
            };
            let has_cluster = slots.iter().any(|slot| &slot.cluster == key);
            // Keep a gap when possible; a smaller aligned neighbor beats a disconnected card.
            let found = [(true, true), (true, false), (false, true), (false, false)]
                .into_iter()
                .filter(|(_, connected)| !*connected || has_cluster)
                .find_map(|(gap, connected)| {
                    spans
                        .iter()
                        .find_map(|span| board.nearest(*span, target, index, gap, connected))
                });
            match found {
                Some(placement) => {
                    board.mark(placement, Use::Card(index));
                    slots.push(Slot {
                        window: window.window,
                        cluster: key.clone(),
                        placement,
                    });
                }
                None => overflow.push(window.window),
            }
        }
    }

    let mut overflow_card = None;
    if !overflow.is_empty() {
        let pool = if slots.len() > kept { kept.. } else { 0.. };
        let victim = slots[pool.clone()]
            .iter()
            .enumerate()
            .max_by_key(|(_, slot)| (mru_of(slot.window), slot.window))
            .map(|(offset, _)| pool.start + offset);
        if let Some(victim) = victim {
            let slot = slots.remove(victim);
            overflow_card = Some(slot.placement);
            overflow.push(slot.window);
        }
        overflow.sort_by_key(|window| (mru_of(*window), *window));
    }
    Arrangement {
        grid: (cols, rows),
        slots,
        overflow,
        overflow_card,
    }
}

/// Give each window that kept no slot the cells it was dropped on, when its pin is for this grid
/// and those cells are still free.
fn place_pins(
    board: &mut Board,
    slots: &mut Vec<Slot>,
    windows: &[WorkWindow],
    pins: &Pins,
    grid: (u16, u16),
    cluster_index: impl Fn(&ClusterKey) -> usize,
) {
    for window in windows {
        if slots.iter().any(|slot| slot.window == window.window) {
            continue;
        }
        if let Some(pin) = pins.get(&window.window)
            && pin.grid == grid
            && board.is_free(pin.placement)
        {
            board.mark(pin.placement, Use::Card(cluster_index(&window.cluster)));
            slots.push(Slot {
                window: window.window,
                cluster: window.cluster.clone(),
                placement: pin.placement,
            });
        }
    }
}

/// Clusters in order (most recent member first, then key), each with its members by recency.
fn clusters(windows: &[WorkWindow]) -> Vec<(ClusterKey, Vec<&WorkWindow>)> {
    let mut clusters: Vec<(ClusterKey, Vec<&WorkWindow>)> = Vec::new();
    for window in windows {
        match clusters.iter_mut().find(|(key, _)| *key == window.cluster) {
            Some((_, members)) => members.push(window),
            None => clusters.push((window.cluster.clone(), vec![window])),
        }
    }
    for (_, members) in &mut clusters {
        members.sort_by_key(|member| (member.mru, member.window));
    }
    clusters.sort_by(|(left, a), (right, b)| {
        (a[0].mru, a[0].window)
            .cmp(&(b[0].mru, b[0].window))
            .then_with(|| left.cmp(right))
    });
    clusters
}

/// The centroid of one cluster's slots, in doubled cell units, as `(sum_x, sum_y, count)`.
fn centroid(slots: &[Slot], key: &ClusterKey) -> Option<(i64, i64, i64)> {
    let mut sum = (0, 0, 0);
    for slot in slots.iter().filter(|slot| slot.cluster == *key) {
        let (x, y) = doubled_centre(slot.placement);
        sum = (sum.0 + x, sum.1 + y, sum.2 + 1);
    }
    (sum.2 > 0).then_some(sum)
}

/// A placement's centre in doubled cell units, so it stays an integer.
fn doubled_centre(placement: Placement) -> (i64, i64) {
    (
        2 * i64::from(placement.cell.col) + i64::from(placement.span.cols),
        2 * i64::from(placement.cell.row) + i64::from(placement.span.rows),
    )
}

fn clamp(span: Span, cols: u16, rows: u16) -> Span {
    Span::new(span.cols.min(cols), span.rows.min(rows))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Use {
    Free,
    Fixed,
    Card(usize),
}

struct Board {
    cols: u16,
    rows: u16,
    cells: Vec<Use>,
}

impl Board {
    fn new(cols: u16, rows: u16, occupied: &[Placement]) -> Self {
        let mut board = Self {
            cols,
            rows,
            cells: vec![Use::Free; usize::from(cols) * usize::from(rows)],
        };
        for placement in occupied {
            board.mark(*placement, Use::Fixed);
        }
        board
    }

    fn at(&self, col: u16, row: u16) -> Use {
        self.cells[usize::from(row) * usize::from(self.cols) + usize::from(col)]
    }

    /// Every cell of `placement` that lies on the grid.
    fn cells_of(&self, placement: Placement) -> impl Iterator<Item = (u16, u16)> + use<> {
        let (cols, rows) = (
            placement.cell.col..placement.right().min(self.cols),
            placement.cell.row..placement.bottom().min(self.rows),
        );
        rows.flat_map(move |row| cols.clone().map(move |col| (col, row)))
    }

    fn mark(&mut self, placement: Placement, use_: Use) {
        for (col, row) in self.cells_of(placement) {
            let index = usize::from(row) * usize::from(self.cols) + usize::from(col);
            self.cells[index] = use_;
        }
    }

    fn is_free(&self, placement: Placement) -> bool {
        placement.span.cols > 0
            && placement.span.rows > 0
            && placement.fits(self.cols, self.rows)
            && self
                .cells_of(placement)
                .all(|(col, row)| self.at(col, row) == Use::Free)
    }

    /// Whether the bordering cells touch this cluster and stay clear of other clusters.
    fn neighbors(&self, placement: Placement, own: usize) -> (bool, bool) {
        let ring = Placement::new(
            Cell::new(
                placement.cell.col.saturating_sub(1),
                placement.cell.row.saturating_sub(1),
            ),
            Span::new(
                placement.right().saturating_add(1) - placement.cell.col.saturating_sub(1),
                placement.bottom().saturating_add(1) - placement.cell.row.saturating_sub(1),
            ),
        );
        let (mut clear, mut touches) = (true, false);
        for (col, row) in self.cells_of(ring) {
            if let Use::Card(cluster) = self.at(col, row) {
                if cluster == own {
                    touches = true;
                } else {
                    clear = false;
                }
            }
        }
        (clear, touches)
    }

    /// The free position of `span` whose centre is nearest `target` (doubled units, divided by
    /// its count), ties to the upper row then the left column. `require_gap` keeps a cell clear
    /// of other clusters; `connected` requires an aligned edge or corner against its own cluster.
    fn nearest(
        &self,
        span: Span,
        (sum_x, sum_y, count): (i64, i64, i64),
        cluster: usize,
        require_gap: bool,
        connected: bool,
    ) -> Option<Placement> {
        if span.cols == 0 || span.rows == 0 || span.cols > self.cols || span.rows > self.rows {
            return None;
        }
        let mut best: Option<(i128, Placement)> = None;
        for row in 0..=self.rows - span.rows {
            for col in 0..=self.cols - span.cols {
                let placement = Placement::new(Cell::new(col, row), span);
                if !self.is_free(placement) {
                    continue;
                }
                let aligned_bottom = (row + span.rows).is_multiple_of(CARD.rows);
                if require_gap || connected {
                    let (clear, touches) = self.neighbors(placement, cluster);
                    if (require_gap && !clear) || (connected && (!touches || !aligned_bottom)) {
                        continue;
                    }
                }
                let (x, y) = doubled_centre(placement);
                let (dx, dy) = (x * count - sum_x, y * count - sum_y);
                let distance = i128::from(dx) * i128::from(dx)
                    + i128::from(dy) * i128::from(dy)
                    + if aligned_bottom { 0 } else { 1_i128 << 80 };
                if best.is_none_or(|(nearest, _)| distance < nearest) {
                    best = Some((distance, placement));
                }
            }
        }
        best.map(|(_, placement)| placement)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: u64, folder: Option<&str>, mru: u32) -> WorkWindow {
        WorkWindow {
            window: WindowId(id),
            cluster: folder.map_or(ClusterKey::UnfiledTools, |folder| {
                ClusterKey::Folder(PathBuf::from(folder))
            }),
            mru,
        }
    }

    fn at(col: u16, row: u16, cols: u16, rows: u16) -> Placement {
        Placement::new(Cell::new(col, row), Span::new(cols, rows))
    }

    fn touches(a: Placement, b: Placement) -> bool {
        let cols_meet = a.cell.col <= b.right() && b.cell.col <= a.right();
        let rows_meet = a.cell.row <= b.bottom() && b.cell.row <= a.bottom();
        cols_meet && rows_meet && !a.overlaps(b)
    }

    fn assert_disjoint(arrangement: &Arrangement, occupied: &[Placement]) {
        let mut all: Vec<Placement> = arrangement.slots.iter().map(|s| s.placement).collect();
        all.extend(arrangement.overflow_card);
        for (i, a) in all.iter().enumerate() {
            assert!(a.fits(arrangement.grid.0, arrangement.grid.1));
            assert!(occupied.iter().all(|fixed| !fixed.overlaps(*a)));
            assert!(all[i + 1..].iter().all(|b| !a.overlaps(*b)));
        }
    }

    #[test]
    fn folder_hints_are_soft_and_full_cards_share_row_baselines() {
        let windows = [window(1, Some("/a"), 0), window(2, Some("/a"), 1)];
        let hints = crate::work_positions::WorkPositions(std::collections::BTreeMap::from([(
            PathBuf::from("/a"),
            (50000, 50000),
        )]));
        let layout = arrange_with_hints(
            &Arrangement::default(),
            16,
            10,
            &[],
            &windows,
            &hints,
            &Pins::new(),
        );
        assert_disjoint(&layout, &[]);
        for slot in &layout.slots {
            assert_eq!(slot.placement.cell.row % CARD.rows, 0);
        }
        assert!(
            layout.slots[0].placement.cell.col > 6,
            "memory biases toward the remembered side"
        );
        let blocked = layout
            .slots
            .iter()
            .map(|slot| slot.placement)
            .collect::<Vec<_>>();
        let moved = arrange_with_hints(
            &Arrangement::default(),
            16,
            10,
            &blocked,
            &windows,
            &hints,
            &Pins::new(),
        );
        assert_disjoint(&moved, &blocked);
    }
    #[test]
    fn a_single_window_sits_at_the_centre() {
        let layout = arrange(&Arrangement::default(), 16, 10, &[], &[window(1, None, 0)]);
        assert_eq!(layout.slots.len(), 1);
        assert_eq!(layout.slots[0].placement, at(6, 4, 3, 2));
        assert!(layout.overflow.is_empty());
        assert_eq!(layout.overflow_card, None);
    }

    #[test]
    fn two_clusters_are_each_contiguous_and_separated_by_a_cell() {
        let windows = [
            window(1, Some("/a"), 0),
            window(2, Some("/b"), 1),
            window(3, Some("/a"), 2),
            window(4, Some("/b"), 3),
        ];
        let layout = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        assert_disjoint(&layout, &[]);
        let place = |id| layout.slot(WindowId(id)).unwrap().placement;
        assert!(touches(place(1), place(3)), "{:?} {:?}", place(1), place(3));
        assert!(touches(place(2), place(4)), "{:?} {:?}", place(2), place(4));
        for a in [1, 3] {
            for b in [2, 4] {
                assert!(!touches(place(a), place(b)), "clusters keep a cell apart");
            }
        }
    }

    #[test]
    fn the_most_recent_cluster_is_nearest_the_centre() {
        let windows = [
            window(7, Some("/old"), 5),
            window(8, Some("/new"), 0),
            window(9, None, 3),
        ];
        let layout = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        let distance = |id| {
            let (x, y) = doubled_centre(layout.slot(WindowId(id)).unwrap().placement);
            (x - 16).pow(2) + (y - 10).pow(2)
        };
        assert_eq!(
            layout.slot(WindowId(8)).unwrap().placement,
            at(6, 4, 3, 2),
            "an odd-width card straddles the centre, ties to the left"
        );
        assert!(distance(8) < distance(9) && distance(8) < distance(7));
    }

    #[test]
    fn persistent_tiles_are_never_covered() {
        let occupied = [at(5, 3, 5, 4), at(0, 0, 16, 2)];
        let windows: Vec<_> = (1..=6).map(|id| window(id, None, id as u32)).collect();
        let layout = arrange(&Arrangement::default(), 16, 10, &occupied, &windows);
        assert_eq!(layout.slots.len(), 6);
        assert_disjoint(&layout, &occupied);
    }

    #[test]
    fn recency_and_parking_keep_every_slot() {
        let mut windows = vec![
            window(1, Some("/a"), 0),
            window(2, Some("/b"), 1),
            window(3, None, 2),
        ];
        let first = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        // The least recent becomes the most recent, as an activation or a restore does.
        windows[0].mru = 2;
        windows[2].mru = 0;
        let second = arrange(&first, 16, 10, &[], &windows);
        assert_eq!(first.slots, second.slots);
    }

    #[test]
    fn a_new_window_joins_its_folders_cluster() {
        let mut windows = vec![window(1, Some("/a"), 1), window(2, Some("/b"), 2)];
        let first = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        windows.push(window(3, Some("/a"), 0));
        let second = arrange(&first, 16, 10, &[], &windows);
        assert_eq!(second.slot(WindowId(1)), first.slot(WindowId(1)));
        assert_eq!(second.slot(WindowId(2)), first.slot(WindowId(2)));
        let new = second.slot(WindowId(3)).unwrap().placement;
        assert!(touches(new, first.slot(WindowId(1)).unwrap().placement));
        assert!(!touches(new, first.slot(WindowId(2)).unwrap().placement));
    }

    #[test]
    fn windows_that_fit_nowhere_are_counted_on_one_overflow_card() {
        // On a 6×2 grid the first card takes the centre (cols 1–3), two small cards fit in
        // cols 4–5, and col 0 is too narrow for anything: two windows have nowhere to go, and
        // the least recent card placed in this pass gives up its cells to count them.
        let windows: Vec<_> = (1..=5).map(|id| window(id, None, id as u32)).collect();
        let layout = arrange(&Arrangement::default(), 6, 2, &[], &windows);
        assert_eq!(
            layout
                .slots
                .iter()
                .map(|slot| slot.window)
                .collect::<Vec<_>>(),
            vec![WindowId(1), WindowId(2)]
        );
        assert_eq!(layout.overflow, vec![WindowId(3), WindowId(4), WindowId(5)]);
        layout.overflow_card.expect("overflow has a reachable card");
        assert_disjoint(&layout, &[]);
        // Recomputing over itself is stable.
        let again = arrange(&layout, 6, 2, &[], &windows);
        assert_eq!(again.slots, layout.slots);
        assert_eq!(again.overflow_card, layout.overflow_card);
        // No room at all: every window overflows and there is no card.
        let full = arrange(&Arrangement::default(), 6, 2, &[at(0, 0, 6, 2)], &windows);
        assert!(full.slots.is_empty());
        assert_eq!(full.overflow.len(), 5);
        assert_eq!(full.overflow_card, None);
    }

    #[test]
    fn a_small_card_is_used_only_when_a_full_one_cannot_fit() {
        // Two free cells in the top row only.
        let occupied = [at(2, 0, 2, 2), at(0, 1, 2, 1)];
        let layout = arrange(
            &Arrangement::default(),
            4,
            2,
            &occupied,
            &[window(1, None, 0)],
        );
        assert_eq!(layout.slots[0].placement, at(0, 0, 2, 1));
    }

    #[test]
    fn a_grid_change_rearranges_from_scratch() {
        let windows = [window(1, None, 0)];
        let first = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        let moved = Arrangement {
            slots: vec![Slot {
                placement: at(0, 0, 3, 2),
                ..first.slots[0].clone()
            }],
            ..first.clone()
        };
        assert_eq!(arrange(&moved, 16, 10, &[], &windows).slots, moved.slots);
        let bigger = arrange(&moved, 20, 12, &[], &windows);
        assert_eq!(bigger.grid, (20, 12));
        assert_ne!(bigger.slots[0].placement, moved.slots[0].placement);
        assert_disjoint(&bigger, &[]);
    }

    #[test]
    fn a_tile_placed_over_a_card_moves_only_that_card() {
        let windows = [window(1, Some("/a"), 0), window(2, Some("/b"), 1)];
        let first = arrange(&Arrangement::default(), 16, 10, &[], &windows);
        let covered = first.slot(WindowId(1)).unwrap().placement;
        let second = arrange(&first, 16, 10, &[covered], &windows);
        assert_eq!(second.slot(WindowId(2)), first.slot(WindowId(2)));
        assert!(
            !second
                .slot(WindowId(1))
                .unwrap()
                .placement
                .overlaps(covered)
        );
    }

    #[test]
    fn a_moved_card_lands_only_on_free_cells_and_others_stay_put() {
        let windows = [window(1, Some("/a"), 0), window(2, Some("/b"), 1)];
        let occupied = [at(0, 0, 2, 2)];
        let mut layout = arrange(&Arrangement::default(), 16, 10, &occupied, &windows);
        let before = layout.clone();
        let other = layout.slot(WindowId(2)).unwrap().placement;
        // Onto a persistent tile, onto another card, or off the grid: refused, nothing moves.
        for refused in [
            at(1, 1, 3, 2),
            Placement::new(other.cell, CARD),
            at(15, 9, 3, 2),
        ] {
            assert!(!layout.move_card(WindowId(1), refused, &occupied));
            assert_eq!(layout, before);
        }
        // One cell over, away from the other card, overlapping its own old cells: allowed, and
        // only the moved card changes.
        let own = layout.slot(WindowId(1)).unwrap().placement;
        let col = if other.cell.col > own.cell.col {
            own.cell.col - 1
        } else {
            own.cell.col + 1
        };
        let nudged = Placement::new(Cell::new(col, own.cell.row), own.span);
        assert!(nudged.overlaps(own));
        assert!(layout.move_card(WindowId(1), nudged, &occupied));
        assert_eq!(layout.slot(WindowId(1)).unwrap().placement, nudged);
        assert_eq!(layout.slot(WindowId(2)).unwrap().placement, other);
        assert!(
            !layout.move_card(WindowId(1), nudged, &occupied),
            "unchanged is no move"
        );
        assert!(layout.move_card(WindowId(1), at(13, 8, 3, 2), &occupied));
        assert_eq!(layout.slot(WindowId(1)).unwrap().placement, at(13, 8, 3, 2));
        assert_eq!(layout.slot(WindowId(2)).unwrap().placement, other);
        assert_disjoint(&layout, &occupied);
    }

    #[test]
    fn a_pin_places_a_card_that_needs_cells_while_they_are_free_on_that_grid() {
        let windows = [window(1, None, 0), window(2, None, 1)];
        let pins = Pins::from([(
            WindowId(1),
            Pin {
                grid: (16, 10),
                placement: at(12, 7, 3, 2),
            },
        )]);
        let hints = crate::work_positions::WorkPositions::default();
        let fresh = Arrangement::default();
        let pinned = arrange_with_hints(&fresh, 16, 10, &[], &windows, &hints, &pins);
        assert_eq!(pinned.slot(WindowId(1)).unwrap().placement, at(12, 7, 3, 2));
        assert_disjoint(&pinned, &[]);
        // A tile on the remembered cells, or another grid: the card is placed as usual.
        let covered =
            arrange_with_hints(&fresh, 16, 10, &[at(13, 8, 1, 1)], &windows, &hints, &pins);
        assert!(
            !covered
                .slot(WindowId(1))
                .unwrap()
                .placement
                .overlaps(at(13, 8, 1, 1))
        );
        let other_grid = arrange_with_hints(&fresh, 20, 12, &[], &windows, &hints, &pins);
        assert_eq!(
            other_grid.slot(WindowId(1)).unwrap().placement,
            arrange(&fresh, 20, 12, &[], &windows)
                .slot(WindowId(1))
                .unwrap()
                .placement
        );
    }
}
