//! The switcher's frozen list.
//!
//! `Alt+Tab` commits on release: stepping only moves the selection, and the window under it is
//! activated when the modifier comes up. The list is still built once at the first press and
//! walked until then, so the rows never move under the user's hands while the chord is held and
//! the HUD always draws exactly the list being walked. This type is that frozen list; the
//! compositor drops it on release, on cancel, and prunes a window from it when the window goes
//! away (`templar-comp/src/keybindings.rs`). Window eligibility and order come from semantic
//! records, never from `LayerStack`.

use crate::WindowId;

/// A switcher list and where in it the walk currently is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRing {
    targets: Vec<WindowId>,
    /// `None` until the first step: with nothing focused the walk starts *on* the top window
    /// rather than one past it.
    index: Option<usize>,
}

impl SwitchRing {
    /// Freeze `targets` (top-most first) with the walk starting at `current`. `None` for an empty
    /// list, so a caller never has to ask whether there is anything to switch to.
    pub fn new(targets: Vec<WindowId>, current: Option<WindowId>) -> Option<Self> {
        if targets.is_empty() {
            return None;
        }
        let index = current.and_then(|id| targets.iter().position(|target| *target == id));
        Some(Self { targets, index })
    }

    /// One step forward (or backward), wrapping. The first forward step from the focused window
    /// lands on the one focused before it, which is the whole point of the chord.
    pub fn step(&mut self, forward: bool) -> WindowId {
        let len = self.targets.len();
        let next = match self.index {
            None if forward => 0,
            None => len - 1,
            Some(index) if forward => (index + 1) % len,
            Some(index) => (index + len - 1) % len,
        };
        self.index = Some(next);
        self.targets[next]
    }

    pub fn selected(&self) -> Option<WindowId> {
        self.index.map(|index| self.targets[index])
    }

    /// The frozen list, for the HUD that draws it.
    pub fn targets(&self) -> &[WindowId] {
        &self.targets
    }

    /// The ring without `window_id` — a window that went away mid-walk. The selection stays on
    /// the same window when that window survives; when the selected window itself is removed,
    /// the one after it (wrapping) becomes the selection, so a release still lands somewhere the
    /// user could see in the list. `None` once nothing is left, exactly as [`Self::new`] answers
    /// an empty list.
    #[must_use]
    pub fn remove(mut self, window_id: WindowId) -> Option<Self> {
        let Some(position) = self.targets.iter().position(|id| *id == window_id) else {
            return Some(self);
        };
        self.targets.remove(position);
        if self.targets.is_empty() {
            return None;
        }
        self.index = self.index.map(|index| match index.cmp(&position) {
            std::cmp::Ordering::Greater => index - 1,
            std::cmp::Ordering::Less => index,
            std::cmp::Ordering::Equal => position % self.targets.len(),
        });
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(raw: &[u64]) -> Vec<WindowId> {
        raw.iter().copied().map(WindowId).collect()
    }

    #[test]
    fn the_first_step_is_the_previously_focused_window_and_the_walk_goes_on_from_there() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), Some(WindowId(1))).unwrap();
        assert_eq!(ring.step(true), WindowId(2));
        assert_eq!(ring.step(true), WindowId(3));
        // Wraps rather than stopping at the end.
        assert_eq!(ring.step(true), WindowId(1));
        assert_eq!(ring.step(false), WindowId(3));
        assert_eq!(ring.selected(), Some(WindowId(3)));
    }

    #[test]
    fn nothing_focused_starts_on_the_top_window_not_one_past_it() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), None).unwrap();
        assert_eq!(ring.step(true), WindowId(1));
        let mut backwards = SwitchRing::new(ids(&[1, 2, 3]), None).unwrap();
        assert_eq!(backwards.step(false), WindowId(3));
    }

    #[test]
    fn a_window_that_is_not_in_the_list_starts_from_the_top() {
        let mut ring = SwitchRing::new(ids(&[1, 2]), Some(WindowId(7))).unwrap();
        assert_eq!(ring.step(true), WindowId(1));
    }

    #[test]
    fn nothing_to_switch_to_is_no_ring_at_all() {
        assert!(SwitchRing::new(Vec::new(), None).is_none());
    }

    #[test]
    fn removing_the_selected_window_selects_the_one_after_it() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), Some(WindowId(1))).unwrap();
        ring.step(true);
        let ring = ring.remove(WindowId(2)).unwrap();
        assert_eq!(ring.targets(), ids(&[1, 3]).as_slice());
        assert_eq!(ring.selected(), Some(WindowId(3)));
    }

    #[test]
    fn removing_the_selected_last_window_wraps_to_the_top() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3]), Some(WindowId(1))).unwrap();
        ring.step(false);
        assert_eq!(ring.selected(), Some(WindowId(3)));
        let ring = ring.remove(WindowId(3)).unwrap();
        assert_eq!(ring.selected(), Some(WindowId(1)));
    }

    #[test]
    fn removing_another_window_keeps_the_selection_on_the_same_window() {
        let mut ring = SwitchRing::new(ids(&[1, 2, 3, 4]), Some(WindowId(1))).unwrap();
        ring.step(true);
        ring.step(true);
        assert_eq!(ring.selected(), Some(WindowId(3)));
        // Before the selection: the index shifts, the window does not.
        let mut ring = ring.remove(WindowId(1)).unwrap();
        assert_eq!(ring.selected(), Some(WindowId(3)));
        // After it: nothing moves.
        ring = ring.remove(WindowId(4)).unwrap();
        assert_eq!(ring.selected(), Some(WindowId(3)));
        assert_eq!(ring.step(true), WindowId(2));
        // Not in the list at all: unchanged.
        let unchanged = ring.clone().remove(WindowId(9)).unwrap();
        assert_eq!(unchanged, ring);
    }

    #[test]
    fn removing_the_last_window_leaves_no_ring() {
        let mut ring = SwitchRing::new(ids(&[5]), None).unwrap();
        ring.step(true);
        assert!(ring.remove(WindowId(5)).is_none());
    }
}
