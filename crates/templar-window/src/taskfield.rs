//! Taskfield's layout: where each window is drawn while the compositor is presenting them.
//!
//! **Taskfield is a view, not a place.** HomeGarden is the desktop — persistent, arrangeable, full
//! of the user's own things. Taskfield is a temporary projection of the windows that already exist:
//! it is computed when the mode opens, it is discarded when the mode closes, and nothing here is
//! ever stored against a window. There is deliberately no `save_layout`, no
//! `window.taskfield_position` and no user-arrangeable geometry, because the moment a card's
//! position acquires permanent meaning this has become a second desktop.
//!
//! So the layout is a *generated* one, in the shape the eye can read as generated: one section per
//! task unit, cards in rows inside it, per output. Not a weighted spatial packer preserving each
//! window's real position — that would look like a desktop, and would move every card when one
//! window opened.
//!
//! Pure, like the rest of this crate: the compositor lays out and draws Taskfield with the shell
//! dead, and the shell — when it is alive — runs *this same function* over the toplevels it holds
//! to place its own labels over the same cards.

use std::collections::{BTreeMap, HashSet};

use crate::{
    Output, Rect, ShellToplevel, TaskKey, TaskUnit,
    geometry::{Direction, choose_target_output, nearest_window},
};

/// The smallest cell a card is laid out in. Below this a card stops being a picture of a window
/// and becomes a coloured square, which is what the recovery scatter already was.
pub const MIN_CARD: (i32, i32) = (160, 90);

/// The inset from the output's edges.
const MARGIN: i32 = 24;
/// Between cells, and between a section and the next section's header.
const GAP: i32 = 12;
/// The band a section's name, accent and count are drawn in.
const HEADER_HEIGHT: i32 = 28;

/// Which windows Taskfield is showing. Set by whoever opened the mode and changed only by an
/// explicit act; a typed query *dims* non-matches rather than re-filtering, so the cards under the
/// pointer never move while the user is looking at them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TaskFilter {
    /// Every window on screen. The chord, the corner, and `window.show-all`.
    #[default]
    All,
    /// One unit: a fan overflowing, a chip with more than [`crate::task::FAN_MAX`] windows, or the
    /// launcher's `→` on a running application.
    Unit(TaskKey),
    /// An explicit list: a HomeGarden place's "N windows active". The compositor never learns what
    /// a place is — the shell sends the ids.
    Windows(Vec<crate::WindowId>),
    /// The shelf band: parked windows only.
    Shelved,
}

/// One window, drawn at `rect` instead of its real geometry. Nothing else about the window
/// changes: no `xdg_toplevel.configure` is sent, the client keeps painting at its real size, and
/// leaving the mode is a render change rather than a geometry change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Card {
    pub window: crate::WindowId,
    pub rect: Rect,
}

/// One task unit's band of cards on one output. A unit whose windows are on two outputs gets one
/// section on each — a window does not jump screens to be found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Section {
    pub key: TaskKey,
    pub label: String,
    pub accent: Option<u32>,
    /// The band the name, accent and count are drawn in, always inside the output.
    pub header: Rect,
    pub cards: Vec<Card>,
    /// Frozen cell rectangles, reading order. Paging paints different windows into these without
    /// moving the cells, so a card never slides out from under the pointer as a side effect.
    pub cells: Vec<Rect>,
    /// Every window of this unit on this output, in pack order. Paging walks this list.
    pub windows: Vec<crate::WindowId>,
    /// Current page, zero-based. Wrapping is [`page`]'s job.
    pub page: usize,
    /// Windows of this unit on this output that are not on the current page, in pack order.
    pub rest: Vec<crate::WindowId>,
    /// How many of the unit's windows on this output did not fit on this page. Drawn as `+n more`.
    /// Equal to [`Self::rest`]'s length; paging swaps `rest` into [`Self::cards`].
    pub overflow: usize,
}

/// Everything the mode is, and all of it thrown away on exit.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskfieldState {
    /// The window that was activated when the mode opened, so `Escape` can go back to it.
    pub entered_from: Option<crate::WindowId>,
    pub filter: TaskFilter,
    pub query: String,
    pub selected: Option<crate::WindowId>,
    pub sections: Vec<Section>,
    /// Windows that answer the current query, or `None` when the query is empty (everything
    /// matches). Filled by `WindowService` so dimming and selection share one set.
    pub matching: Option<HashSet<crate::WindowId>>,
}

impl TaskfieldState {
    /// The card for `window`, if it is on screen in this mode.
    #[must_use]
    pub fn card(&self, window: crate::WindowId) -> Option<Card> {
        self.sections
            .iter()
            .flat_map(|section| section.cards.iter())
            .find(|card| card.window == window)
            .copied()
    }

    /// Every card, in reading order across every section and output.
    pub fn cards(&self) -> impl Iterator<Item = Card> + '_ {
        self.sections
            .iter()
            .flat_map(|section| section.cards.iter().copied())
    }

    /// Whether `window` answers the query. An empty query matches everything.
    #[must_use]
    pub fn matches(&self, window: crate::WindowId) -> bool {
        self.matching
            .as_ref()
            .is_none_or(|matching| matching.contains(&window))
    }
}

/// Lay `units` out for `filter`: one section per unit per output, cards in rows, largest cells that
/// fit. Every rectangle returned lies inside the output it was placed on.
///
/// With no outputs nothing can be shown anywhere, and the answer is no sections at all — the same
/// answer [`crate::scatter_layout`] gives.
#[must_use]
pub fn layout(
    units: &[TaskUnit],
    toplevels: &[ShellToplevel],
    outputs: &[Output],
    filter: &TaskFilter,
) -> Vec<Section> {
    if outputs.is_empty() {
        return Vec::new();
    }
    let scope = in_scope(units, toplevels, filter);
    let mut out = Vec::new();
    for output in outputs {
        let members: Vec<(&TaskUnit, Vec<&ShellToplevel>)> = units
            .iter()
            .filter_map(|unit| {
                let mine: Vec<&ShellToplevel> = unit
                    .windows
                    .iter()
                    .filter_map(|id| scope.get(id).copied())
                    .filter(|top| choose_target_output(top.geometry, outputs).name == output.name)
                    .collect();
                (!mine.is_empty()).then_some((unit, mine))
            })
            .collect();
        if !members.is_empty() {
            out.extend(pack(&members, output));
        }
    }
    out
}

/// The next selection from `from` in `direction`, over the frozen layout in reading order.
/// `None` selected starts at the first card; a step off the edge stays where it is.
#[must_use]
pub fn step(
    sections: &[Section],
    from: Option<crate::WindowId>,
    direction: Direction,
) -> Option<crate::WindowId> {
    let cards: Vec<(crate::WindowId, Rect)> = sections
        .iter()
        .flat_map(|section| section.cards.iter())
        .map(|card| (card.window, card.rect))
        .collect();
    let Some(current) = from else {
        return cards.first().map(|(id, _)| *id);
    };
    // Left and right also walk *between* sections, because the rows are the reading order; up and
    // down are spatial, so a card above in the previous section is reachable.
    if let Some(next) = nearest_window(&cards, current, direction) {
        return Some(next);
    }
    let index = cards.iter().position(|(id, _)| *id == current)?;
    match direction {
        Direction::Right | Direction::Down => cards.get(index + 1).map(|(id, _)| *id),
        Direction::Left | Direction::Up => index
            .checked_sub(1)
            .and_then(|previous| cards.get(previous))
            .map(|(id, _)| *id),
    }
}

/// [`step`], skipping windows `matching` rejects. An empty query is `matching` always true —
/// the cards do not move; only which one is selected does.
#[must_use]
pub fn step_matching(
    sections: &[Section],
    from: Option<crate::WindowId>,
    direction: Direction,
    matching: impl Fn(crate::WindowId) -> bool,
) -> Option<crate::WindowId> {
    let mut current = from;
    loop {
        let next = step(sections, current, direction)?;
        if matching(next) {
            return Some(next);
        }
        current = Some(next);
    }
}

/// Which overflowing section's header contains `pos`, if any. The compositor's overflow click
/// and PageDown share this so a header and a key cannot disagree about whose rest they show.
#[must_use]
pub fn overflow_at(sections: &[Section], pos: crate::Point) -> Option<usize> {
    sections.iter().position(|section| {
        section.overflow > 0
            && pos.x >= section.header.x
            && pos.x < section.header.right()
            && pos.y >= section.header.y
            && pos.y < section.header.bottom()
    })
}

/// Advance `section` by one page: the next `cells.len()` windows occupy the same frozen cells.
/// Wraps to the first page after the last. `false` when there is nothing to show.
pub fn page(section: &mut Section, geometry_of: impl Fn(crate::WindowId) -> Option<Rect>) -> bool {
    let n = section.cells.len();
    if n == 0 || section.windows.len() <= n {
        return false;
    }
    let pages = section.windows.len().div_ceil(n);
    section.page = (section.page + 1) % pages;
    let start = section.page.saturating_mul(n);
    let end = (start + n).min(section.windows.len());
    let shown = &section.windows[start.min(section.windows.len())..end];
    section.cards = shown
        .iter()
        .zip(section.cells.iter())
        .filter_map(|(id, cell)| {
            geometry_of(*id).map(|geometry| Card {
                window: *id,
                rect: card_in(*cell, geometry),
            })
        })
        .collect();
    section.rest = section
        .windows
        .iter()
        .enumerate()
        .filter(|(index, _)| *index < start || *index >= end)
        .map(|(_, id)| *id)
        .collect();
    section.overflow = section.rest.len();
    true
}

/// The windows `filter` admits, by id.
fn in_scope<'a>(
    units: &[TaskUnit],
    toplevels: &'a [ShellToplevel],
    filter: &TaskFilter,
) -> BTreeMap<crate::WindowId, &'a ShellToplevel> {
    toplevels
        .iter()
        .filter(|top| match filter {
            TaskFilter::All => !top.shelved,
            TaskFilter::Shelved => top.shelved,
            TaskFilter::Windows(ids) => ids.contains(&top.window_id),
            TaskFilter::Unit(key) => {
                !top.shelved
                    && units
                        .iter()
                        .any(|unit| unit.key == *key && unit.windows.contains(&top.window_id))
            }
        })
        .map(|top| (top.window_id, top))
        .collect()
}

/// Stack `members`' sections down one output, choosing the largest cell size whose total height
/// fits. What does not fit is counted, never dropped silently.
fn pack(members: &[(&TaskUnit, Vec<&ShellToplevel>)], output: &Output) -> Vec<Section> {
    let full = output.logical;
    let area = Rect::new(
        full.x + MARGIN,
        full.y + MARGIN,
        (full.width - 2 * MARGIN).max(MIN_CARD.0),
        (full.height - 2 * MARGIN).max(MIN_CARD.1 + HEADER_HEIGHT),
    );
    let max_columns = ((area.width + GAP) / (MIN_CARD.0 + GAP)).max(1);

    let mut chosen = (max_columns, cell_size(area.width, max_columns));
    for columns in 1..=max_columns {
        let (_, cell_height) = cell_size(area.width, columns);
        let height: i32 = members
            .iter()
            .map(|(_, mine)| HEADER_HEIGHT + rows(mine.len(), columns) * (cell_height + GAP) + GAP)
            .sum();
        if height <= area.height {
            chosen = (columns, cell_size(area.width, columns));
            break;
        }
    }
    let (columns, (cell_width, cell_height)) = chosen;

    let mut out = Vec::with_capacity(members.len());
    let mut y = area.y;
    for (unit, mine) in members {
        // A header always lands inside the output, even once the budget is spent: the mode must
        // never draw outside the screen, and a section with no room still says how much it holds.
        let header_y = y.min(full.bottom() - HEADER_HEIGHT);
        y += HEADER_HEIGHT;

        let mut cards = Vec::new();
        let mut cells = Vec::new();
        let mut rest = Vec::new();
        for (index, toplevel) in mine.iter().enumerate() {
            let column = i32::try_from(index).unwrap_or(i32::MAX) % columns;
            let row = i32::try_from(index).unwrap_or(i32::MAX) / columns;
            let cell = Rect::new(
                area.x + column * (cell_width + GAP),
                y + row * (cell_height + GAP),
                cell_width,
                cell_height,
            );
            if cell.bottom() > area.bottom() {
                rest.push(toplevel.window_id);
                continue;
            }
            cells.push(cell);
            cards.push(Card {
                window: toplevel.window_id,
                rect: card_in(cell, toplevel.geometry),
            });
        }
        y += rows(cards.len(), columns) * (cell_height + GAP) + GAP;

        let windows: Vec<crate::WindowId> = mine.iter().map(|top| top.window_id).collect();
        out.push(Section {
            key: unit.key.clone(),
            label: unit.label.clone(),
            accent: unit.accent,
            header: Rect::new(area.x, header_y, area.width, HEADER_HEIGHT),
            overflow: rest.len(),
            cells,
            windows,
            page: 0,
            rest,
            cards,
        });
    }
    out
}

/// Cell width and height for `columns` across `width`, never below [`MIN_CARD`]. Cells are
/// 16:9-ish so a section of one row is a band rather than a wall.
fn cell_size(width: i32, columns: i32) -> (i32, i32) {
    let cell_width = ((width - GAP * (columns - 1)) / columns).max(MIN_CARD.0);
    let cell_height = (cell_width * 9 / 16).max(MIN_CARD.1);
    (cell_width, cell_height)
}

fn rows(cards: usize, columns: i32) -> i32 {
    let columns = columns.max(1);
    let cards = i32::try_from(cards).unwrap_or(i32::MAX);
    (cards + columns - 1) / columns
}

/// `real` scaled to fit `cell`, keeping its aspect, **never enlarged** — a card is a picture of a
/// window, so a small window stays small rather than being blown up to fill a cell — and centred.
fn card_in(cell: Rect, real: Rect) -> Rect {
    let (rw, rh) = (i64::from(real.width.max(1)), i64::from(real.height.max(1)));
    let (cw, ch) = (i64::from(cell.width.max(1)), i64::from(cell.height.max(1)));
    let (mut width, mut height) = if cw * rh <= ch * rw {
        (cw, (cw * rh) / rw)
    } else {
        ((ch * rw) / rh, ch)
    };
    if width > rw || height > rh {
        (width, height) = (rw, rh);
    }
    let width = i32::try_from(width.max(1)).unwrap_or(cell.width);
    let height = i32::try_from(height.max(1)).unwrap_or(cell.height);
    Rect::new(
        cell.x + (cell.width - width) / 2,
        cell.y + (cell.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Layer, WindowId, task, test_support::arb_topology};
    use proptest::prelude::*;

    fn top(id: u64, app: &str, geometry: Rect) -> ShellToplevel {
        ShellToplevel {
            window_id: WindowId(id),
            canonical_app_id: Some(app.parse().expect("a valid application id")),
            instance_id: None,
            title: format!("window {id}"),
            verified_identity: true,
            activated: false,
            mru_rank: u32::try_from(id).unwrap_or(0),
            fullscreen: false,
            shelved: false,
            maximized: false,
            geometry,
            layer: Layer::Normal,
            group: None,
        }
    }

    fn one_output() -> Vec<Output> {
        vec![Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000)]
    }

    fn windows(count: u64, app: &str) -> Vec<ShellToplevel> {
        (1..=count)
            .map(|id| {
                top(
                    id,
                    app,
                    Rect::new(i32::try_from(id).unwrap_or(0) * 10, 0, 800, 600),
                )
            })
            .collect()
    }

    fn laid_out(
        toplevels: &[ShellToplevel],
        outputs: &[Output],
        filter: &TaskFilter,
    ) -> Vec<Section> {
        layout(&task::units(toplevels), toplevels, outputs, filter)
    }

    #[test]
    fn one_section_per_unit_in_the_models_order() {
        let mut toplevels = windows(2, "org.templaros.Files");
        toplevels.extend(windows(1, "org.templaros.Term").into_iter().map(|mut top| {
            top.window_id = WindowId(9);
            top
        }));
        let sections = laid_out(&toplevels, &one_output(), &TaskFilter::All);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].key.wire(), "org.templaros.Files");
        assert_eq!(sections[0].cards.len(), 2);
        assert_eq!(sections[1].key.wire(), "org.templaros.Term");
    }

    #[test]
    fn nothing_to_show_is_no_layout_at_all() {
        assert!(laid_out(&windows(2, "org.templaros.Files"), &[], &TaskFilter::All).is_empty());
        assert!(laid_out(&[], &one_output(), &TaskFilter::All).is_empty());
    }

    #[test]
    fn a_filter_selects_what_the_mode_was_opened_over() {
        let mut toplevels = windows(3, "org.templaros.Files");
        toplevels[2].shelved = true;

        let all = laid_out(&toplevels, &one_output(), &TaskFilter::All);
        assert_eq!(all[0].cards.len(), 2, "a parked window is not on screen");

        let shelved = laid_out(&toplevels, &one_output(), &TaskFilter::Shelved);
        assert_eq!(shelved[0].cards.len(), 1);

        let listed = laid_out(
            &toplevels,
            &one_output(),
            &TaskFilter::Windows(vec![WindowId(1)]),
        );
        assert_eq!(listed[0].cards.len(), 1);
        assert_eq!(listed[0].cards[0].window, WindowId(1));

        let unit = laid_out(
            &toplevels,
            &one_output(),
            &TaskFilter::Unit(TaskKey::App("org.templaros.Term".parse().expect("valid"))),
        );
        assert!(unit.is_empty(), "a unit with no windows shows nothing");
    }

    #[test]
    fn each_window_is_laid_out_on_the_output_it_is_already_on() {
        let outputs = vec![
            Output::new("eDP-1", Rect::new(0, 0, 1920, 1080), 1000),
            Output::new("HDMI-1", Rect::new(1920, 0, 1920, 1080), 1000),
        ];
        let toplevels = vec![
            top(1, "org.templaros.Files", Rect::new(100, 100, 800, 600)),
            top(2, "org.templaros.Files", Rect::new(2100, 100, 800, 600)),
        ];
        let sections = laid_out(&toplevels, &outputs, &TaskFilter::All);

        assert_eq!(sections.len(), 2, "one section per output for one unit");
        assert!(sections[0].cards[0].rect.x < 1920);
        assert!(sections[1].cards[0].rect.x >= 1920);
    }

    #[test]
    fn no_card_exceeds_its_window_or_falls_below_the_floor() {
        let toplevels = vec![
            top(1, "org.templaros.Files", Rect::new(0, 0, 120, 60)),
            top(2, "org.templaros.Files", Rect::new(0, 0, 3000, 2000)),
        ];
        let sections = laid_out(&toplevels, &one_output(), &TaskFilter::All);
        let cards = &sections[0].cards;

        assert_eq!(
            (cards[0].rect.width, cards[0].rect.height),
            (120, 60),
            "a small window is never blown up to fill its cell"
        );
        assert!(cards[1].rect.width >= MIN_CARD.0 && cards[1].rect.height >= MIN_CARD.1);
        // Aspect survives the scaling: 3:2 in, 3:2 out, to within integer rounding.
        assert!((cards[1].rect.width * 2 / 3 - cards[1].rect.height).abs() <= 1);
    }

    #[test]
    fn what_does_not_fit_is_counted_rather_than_dropped() {
        let toplevels = windows(400, "org.templaros.Files");
        let sections = laid_out(&toplevels, &one_output(), &TaskFilter::All);
        let section = &sections[0];

        assert!(!section.cards.is_empty());
        assert_eq!(
            section.cards.len() + section.overflow,
            400,
            "every window is either drawn or counted; none vanishes"
        );
        assert_eq!(section.rest.len(), section.overflow);
        assert_eq!(section.cells.len(), section.cards.len());
    }

    #[test]
    fn paging_a_unit_reaches_every_window_on_a_small_output() {
        let output = vec![Output::new("eDP-1", Rect::new(0, 0, 1366, 768), 1000)];
        let toplevels = windows(80, "org.templaros.Files");
        let units = task::units(&toplevels);
        let mut sections = layout(
            &units,
            &toplevels,
            &output,
            &TaskFilter::Unit(units[0].key.clone()),
        );
        assert_eq!(sections.len(), 1);
        let geometry_of = |id: crate::WindowId| {
            toplevels
                .iter()
                .find(|top| top.window_id == id)
                .map(|top| top.geometry)
        };
        let mut seen = std::collections::HashSet::new();
        let cells = sections[0].cells.clone();
        let pages = sections[0]
            .windows
            .len()
            .div_ceil(sections[0].cells.len().max(1));
        for _ in 0..pages {
            for card in &sections[0].cards {
                seen.insert(card.window);
            }
            assert_eq!(
                sections[0].cells, cells,
                "paging must not move the frozen cells"
            );
            if seen.len() == 80 {
                break;
            }
            assert!(page(&mut sections[0], geometry_of));
        }
        assert_eq!(seen.len(), 80, "every window occupied a cell on some page");
    }

    #[test]
    fn stepping_skips_windows_the_query_rejects() {
        let toplevels = windows(4, "org.templaros.Files");
        let sections = laid_out(&toplevels, &one_output(), &TaskFilter::All);
        let first = sections[0].cards[0].window;
        let third = sections[0].cards[2].window;
        let matching = |id: crate::WindowId| id == first || id == third;
        assert_eq!(
            step_matching(&sections, Some(first), Direction::Right, matching),
            Some(third),
            "a non-matching card between two matches is skipped"
        );
        let second = sections[0].cards[1].window;
        assert_ne!(
            step_matching(&sections, Some(first), Direction::Right, matching),
            Some(second)
        );
    }

    #[test]
    fn a_query_change_never_moves_a_card() {
        let toplevels = windows(6, "org.templaros.Files");
        let units = task::units(&toplevels);
        let before = layout(&units, &toplevels, &one_output(), &TaskFilter::All);

        // A query is not an input to the layout at all — it dims, it does not re-place. This is the
        // test that keeps it that way if anyone is ever tempted to filter the input first.
        let matching: Vec<ShellToplevel> = toplevels
            .iter()
            .filter(|top| task::matches(&units[0], top, "window 1"))
            .cloned()
            .collect();
        assert!(matching.len() < toplevels.len(), "the query does narrow");
        assert_eq!(
            before,
            layout(&units, &toplevels, &one_output(), &TaskFilter::All)
        );
    }

    #[test]
    fn stepping_walks_the_cards_and_stops_at_the_edges() {
        let toplevels = windows(4, "org.templaros.Files");
        let sections = laid_out(&toplevels, &one_output(), &TaskFilter::All);
        let first = sections[0].cards[0].window;
        let second = sections[0].cards[1].window;

        assert_eq!(step(&sections, None, Direction::Right), Some(first));
        assert_eq!(step(&sections, Some(first), Direction::Right), Some(second));
        assert_eq!(step(&sections, Some(second), Direction::Left), Some(first));
        assert_eq!(
            step(&sections, Some(first), Direction::Left),
            None,
            "the first card is the left edge"
        );
    }

    proptest! {
        /// The mode may never draw outside the screen: every card and every header lands inside
        /// the output it was placed on, whatever the topology and however many windows there are.
        #[test]
        fn every_card_lies_inside_its_output(
            outputs in arb_topology(),
            count in 1usize..40,
        ) {
            let toplevels: Vec<ShellToplevel> = (0..count)
                .map(|index| {
                    let output = &outputs[index % outputs.len()];
                    let logical = output.logical;
                    top(
                        u64::try_from(index).unwrap_or(0) + 1,
                        "org.templaros.Files",
                        Rect::new(logical.x, logical.y, 800.min(logical.width), 600.min(logical.height)),
                    )
                })
                .collect();

            for section in laid_out(&toplevels, &outputs, &TaskFilter::All) {
                let inside = |rect: Rect| {
                    outputs.iter().any(|output| {
                        let full = output.logical;
                        rect.x >= full.x
                            && rect.y >= full.y
                            && rect.right() <= full.right()
                            && rect.bottom() <= full.bottom()
                    })
                };
                prop_assert!(inside(section.header), "header {:?} escaped", section.header);
                for card in &section.cards {
                    prop_assert!(card.rect.width >= 1 && card.rect.height >= 1);
                    prop_assert!(inside(card.rect), "card {:?} escaped", card.rect);
                }
            }
        }
    }
}
